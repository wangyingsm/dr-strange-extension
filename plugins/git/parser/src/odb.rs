//! The object database, read through the host's two calls.
//!
//! Only what history needs: **commits and annotated tags**. Trees and blobs
//! are never asked for, which is why this can be a few hundred lines rather
//! than a git implementation — no working tree is reconstructed, no diff is
//! taken, and the size of a repository's content never enters into it.
//!
//! Two storages, because git has two. A **loose** object is one zlib stream in
//! one file, named by its oid. A **packed** object lives in a `.pack` beside an
//! `.idx` that maps oid → offset, and may be stored as a *delta* against
//! another object in the same pack (by offset) or against any object at all (by
//! oid). Deltas are what make packs small and what makes reading them work:
//! a commit is usually a small edit of the commit before it.
//!
//! Packs are read whole, because [`Files::read`] is whole-file — there is no
//! seek across the sandbox boundary. That is the one real cost of doing this
//! from inside the sandbox, and it is bounded by the pack's size rather than
//! the repository's history; a pack too large to hold is reported by name
//! instead of failing the run.

use std::io::Read;
use std::rc::Rc;

use crate::Files;

/// What an object is. Blobs are read by nothing here — file *contents* are no
/// part of a history — but the type byte still has to be understood to know
/// that, and a tree is read for the paths a commit changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    fn from_byte(n: u8) -> Option<Kind> {
        Some(match n {
            1 => Kind::Commit,
            2 => Kind::Tree,
            3 => Kind::Blob,
            4 => Kind::Tag,
            _ => return None,
        })
    }

    fn from_word(word: &[u8]) -> Option<Kind> {
        Some(match word {
            b"commit" => Kind::Commit,
            b"tree" => Kind::Tree,
            b"blob" => Kind::Blob,
            b"tag" => Kind::Tag,
            _ => return None,
        })
    }
}

pub struct Object {
    pub kind: Kind,
    pub data: Vec<u8>,
}

/// How deep a delta chain may go before this gives up on it. Real chains are
/// bounded by `pack.depth` (50 by default); this is the guard against a
/// malformed or circular pack, not a limit on honest ones.
const MAX_DELTA_DEPTH: usize = 200;

/// A pack's index, and its data once something has actually been read from it.
struct Pack {
    name: String,
    /// oid (20 bytes) → offset into the pack, sorted by oid as the idx is.
    oids: Vec<[u8; 20]>,
    offsets: Vec<u64>,
    /// `refs/…/pack-xxx.pack`, relative to the git directory.
    pack_path: String,
    /// Loaded on first use and kept: an `Rc` so a delta chain can walk it
    /// while the odb is borrowed mutably to resolve the base.
    data: Option<Rc<Vec<u8>>>,
    /// Said once, when the pack could not be read at all.
    failed: bool,
}

pub struct Odb {
    /// Loose object paths that exist, as `objects/ab/cdef…`.
    loose: std::collections::BTreeSet<String>,
    packs: Vec<Pack>,
    /// Anything a reader would want said about what could not be read.
    pub notes: Vec<String>,
}

impl Odb {
    /// Read every pack index; note the loose objects without reading any.
    pub fn open(files: &dyn Files, listing: &[String]) -> Odb {
        let mut odb = Odb {
            loose: listing
                .iter()
                .filter(|p| is_loose_path(p))
                .cloned()
                .collect(),
            packs: Vec::new(),
            notes: Vec::new(),
        };
        for path in listing.iter().filter(|p| p.ends_with(".idx")) {
            match files
                .read(path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| parse_idx(&bytes).map_err(|e| format!("{path}: {e}")))
            {
                Ok((oids, offsets)) => odb.packs.push(Pack {
                    name: path.clone(),
                    oids,
                    offsets,
                    pack_path: path.trim_end_matches(".idx").to_string() + ".pack",
                    data: None,
                    failed: false,
                }),
                Err(why) => odb.notes.push(format!("pack index unreadable: {why}")),
            }
        }
        if listing.iter().any(|p| p == "objects/info/alternates") {
            odb.notes.push(
                "this repository borrows objects from another one \
                 (objects/info/alternates), which is outside what the host will \
                 answer for — commits stored only there cannot be read"
                    .to_string(),
            );
        }
        odb
    }

    /// One object by oid, or `None` when nothing here holds it.
    pub fn read(&mut self, files: &dyn Files, oid: &str) -> Option<Object> {
        self.read_at_depth(files, oid, 0)
    }

    fn read_at_depth(&mut self, files: &dyn Files, oid: &str, depth: usize) -> Option<Object> {
        if depth > MAX_DELTA_DEPTH {
            return None;
        }
        if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let loose = format!("objects/{}/{}", &oid[..2], &oid[2..]);
        if self.loose.contains(&loose) {
            let raw = files.read(&loose).ok()?;
            return parse_loose(&raw);
        }
        let raw = hex_to_oid(oid)?;
        for i in 0..self.packs.len() {
            if let Some(offset) = find_in_pack(&self.packs[i], &raw) {
                return self.read_from_pack(files, i, offset, depth);
            }
        }
        None
    }

    /// The pack's bytes, read once and kept.
    fn pack_data(&mut self, files: &dyn Files, i: usize) -> Option<Rc<Vec<u8>>> {
        if let Some(data) = &self.packs[i].data {
            return Some(Rc::clone(data));
        }
        if self.packs[i].failed {
            return None;
        }
        match files.read(&self.packs[i].pack_path) {
            Ok(bytes) => {
                let data = Rc::new(bytes);
                self.packs[i].data = Some(Rc::clone(&data));
                Some(data)
            }
            Err(why) => {
                self.packs[i].failed = true;
                let name = self.packs[i].name.clone();
                self.notes.push(format!(
                    "{name}: the pack beside this index could not be read ({why}) — \
                     every object in it is missing from this graph"
                ));
                None
            }
        }
    }

    fn read_from_pack(
        &mut self,
        files: &dyn Files,
        i: usize,
        offset: u64,
        depth: usize,
    ) -> Option<Object> {
        let data = self.pack_data(files, i)?;
        let at = offset as usize;
        let (ty, size, mut cursor) = read_entry_header(&data, at)?;

        match ty {
            1..=4 => Some(Object {
                kind: Kind::from_byte(ty)?,
                data: inflate(&data[cursor..], size)?,
            }),
            // A delta against another object *in this pack*, named by how far
            // back it sits — which is why a pack can be read from any offset
            // without an index of its own.
            6 => {
                let (back, next) = read_offset_delta(&data, cursor)?;
                cursor = next;
                let base_at = offset.checked_sub(back)?;
                let base = self.read_from_pack(files, i, base_at, depth + 1)?;
                let delta = inflate(&data[cursor..], size)?;
                Some(Object {
                    kind: base.kind,
                    data: apply_delta(&base.data, &delta)?,
                })
            }
            // A delta against an object anywhere — possibly loose, possibly in
            // a different pack.
            7 => {
                let base_oid = oid_to_hex(data.get(cursor..cursor + 20)?);
                cursor += 20;
                let base = self.read_at_depth(files, &base_oid, depth + 1)?;
                let delta = inflate(&data[cursor..], size)?;
                Some(Object {
                    kind: base.kind,
                    data: apply_delta(&base.data, &delta)?,
                })
            }
            _ => None,
        }
    }
}

/// `objects/ab/cdef…` — a loose object, rather than `objects/pack/…` or
/// `objects/info/…`.
fn is_loose_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("objects/") else {
        return false;
    };
    let Some((dir, file)) = rest.split_once('/') else {
        return false;
    };
    dir.len() == 2
        && file.len() == 38
        && dir
            .bytes()
            .chain(file.bytes())
            .all(|b| b.is_ascii_hexdigit())
}

/// A loose object: one zlib stream holding `"<kind> <size>\0<payload>"`.
fn parse_loose(raw: &[u8]) -> Option<Object> {
    let all = inflate(raw, 0)?;
    let nul = all.iter().position(|b| *b == 0)?;
    let header = &all[..nul];
    let space = header.iter().position(|b| *b == b' ')?;
    let kind = Kind::from_word(&header[..space])?;
    Some(Object {
        kind,
        data: all[nul + 1..].to_vec(),
    })
}

/// Inflate one zlib stream from the front of `data`, ignoring whatever follows
/// it — inside a pack, what follows is the next object.
fn inflate(data: &[u8], expect: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(expect);
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

// ---- the pack index ------------------------------------------------------

/// Parse a v2 pack index into its oid and offset tables.
///
/// Version 1 is refused rather than guessed at: it has a different layout, git
/// has not written one since 2007, and a wrong offset would produce confident
/// nonsense instead of an error.
fn parse_idx(bytes: &[u8]) -> Result<(Vec<[u8; 20]>, Vec<u64>), String> {
    if bytes.len() < 8 + 256 * 4 {
        return Err("too short to be a pack index".into());
    }
    if &bytes[..4] != b"\xfftOc" {
        return Err("not a v2 pack index (a v1 index has no signature); \
                    `git index-pack` rewrites one"
            .into());
    }
    if be32(bytes, 4) != 2 {
        return Err(format!(
            "pack index version {} is not supported",
            be32(bytes, 4)
        ));
    }
    let count = be32(bytes, 8 + 255 * 4) as usize;
    let oid_table = 8 + 256 * 4;
    let crc_table = oid_table + count * 20;
    let off_table = crc_table + count * 4;
    let big_table = off_table + count * 4;
    if bytes.len() < big_table {
        return Err("truncated pack index".into());
    }

    let mut oids = Vec::with_capacity(count);
    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&bytes[oid_table + i * 20..oid_table + i * 20 + 20]);
        oids.push(oid);

        let raw = be32(bytes, off_table + i * 4);
        // The high bit says "this offset did not fit in 32 bits"; the rest is
        // an index into the table of 64-bit offsets that follows.
        let offset = if raw & 0x8000_0000 == 0 {
            raw as u64
        } else {
            let at = big_table + (raw & 0x7fff_ffff) as usize * 8;
            if bytes.len() < at + 8 {
                return Err("truncated large-offset table".into());
            }
            be64(bytes, at)
        };
        offsets.push(offset);
    }
    Ok((oids, offsets))
}

/// Binary search the index — its oids are sorted, which is the whole point of
/// its shape.
fn find_in_pack(pack: &Pack, oid: &[u8; 20]) -> Option<u64> {
    pack.oids.binary_search(oid).ok().map(|i| pack.offsets[i])
}

// ---- pack entries --------------------------------------------------------

/// An entry's `(type, uncompressed size, offset of the data)`.
///
/// The header is a varint whose *first* byte is laid out differently from the
/// rest: three bits of type and four of size, then seven at a time.
fn read_entry_header(data: &[u8], at: usize) -> Option<(u8, usize, usize)> {
    let mut i = at;
    let first = *data.get(i)?;
    i += 1;
    let ty = (first >> 4) & 0b111;
    let mut size = (first & 0b1111) as usize;
    let mut shift = 4;
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = *data.get(i)?;
        i += 1;
        size |= ((byte & 0x7f) as usize) << shift;
        shift += 7;
    }
    Some((ty, size, i))
}

/// The distance back to a delta's base. Its own encoding, and not the same one
/// as the size varint above: each continuation adds one before shifting, so
/// there is exactly one representation of every distance.
fn read_offset_delta(data: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut i = at;
    let mut byte = *data.get(i)?;
    i += 1;
    let mut value = (byte & 0x7f) as u64;
    while byte & 0x80 != 0 {
        byte = *data.get(i)?;
        i += 1;
        value = ((value + 1) << 7) | (byte & 0x7f) as u64;
    }
    Some((value, i))
}

/// Rebuild an object from its base and a delta.
///
/// The delta is a short program: *copy* a run from the base, or *insert*
/// literal bytes. That is all git stores for an object that resembles one it
/// already has.
fn apply_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut i = 0;
    let base_size = read_varint(delta, &mut i)?;
    if base_size != base.len() {
        return None; // the delta was written against a different object
    }
    let result_size = read_varint(delta, &mut i)?;
    let mut out = Vec::with_capacity(result_size);

    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            // Copy: the low bits say which of the four offset bytes and three
            // size bytes are present; the absent ones are zero.
            let mut offset = 0usize;
            for shift in 0..4 {
                if op & (1 << shift) != 0 {
                    offset |= (*delta.get(i)? as usize) << (shift * 8);
                    i += 1;
                }
            }
            let mut size = 0usize;
            for shift in 0..3 {
                if op & (1 << (4 + shift)) != 0 {
                    size |= (*delta.get(i)? as usize) << (shift * 8);
                    i += 1;
                }
            }
            if size == 0 {
                size = 0x10000; // the one special case in the format
            }
            out.extend_from_slice(base.get(offset..offset.checked_add(size)?)?);
        } else if op != 0 {
            let n = op as usize;
            out.extend_from_slice(delta.get(i..i + n)?);
            i += n;
        } else {
            return None; // 0 is not an instruction
        }
    }
    (out.len() == result_size).then_some(out)
}

/// A little-endian 7-bits-at-a-time varint, as the delta header uses.
fn read_varint(data: &[u8], i: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let byte = *data.get(*i)?;
        *i += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

// ---- bytes ---------------------------------------------------------------

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn be64(bytes: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&bytes[at..at + 8]);
    u64::from_be_bytes(v)
}

pub fn hex_to_oid(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn oid_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}
