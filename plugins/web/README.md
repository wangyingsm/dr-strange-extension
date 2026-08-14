# plugin: web

Parses HTML **and** CSS into graph facts — one plugin for both, manifest
`web@1`, claims `.html .htm .css`. One plugin deliberately: the cross-file
fact a frontend plane wants — *which stylesheet's `.btn` styles which page*
— can only be resolved where one assemble sees both sides. Built on
tree-sitter's html, css **and javascript** grammars (the third for inline
scripts), through the wasi-sdk toolchain.

Restraint is the design: **a page is not its every `<div>`.** The default
facts are the page, its id-carrying elements, its inline-script functions,
and the stylesheet's declared vocabulary. The whole DOM is available as an
opt-in (below).

## Layout

```
parser/     drsg-web-parser — the logic, 16 native tests
component/  drsg-plugin-web — Guest impl + rmp-serde partials (needs wasi-sdk)
```

## Keys — the web's own address where it has one

```
index.html                            a Page
css/site.css                          a Stylesheet
index.html#map                        an id'd element — the URL fragment IS the
                                      platform's own syntax for addressing it
css/site.css::.btn                    a class (selector spelling kept — ::.btn
                                      can never read as a CSS pseudo-element)
css/site.css::--brand                 a custom property
index.html::initMap                   an inline-script function (file-scoped,
                                      the family's {file}::{name} form)
index.html::.local                    a class from the page's own <style>
```

## Nodes

| Label | Emitted for | Props beyond `file` / `line` |
|---|---|---|
| `Page` | each html file | `title` |
| `Stylesheet` | each css file | `rule_count`; `classes` and `custom_properties` as comma-joined **key lists** — the dashboard expands them and follows each entry as a link. Minified files skip the lists |
| `Element` | id-carrying elements — **nested the way the DOM nests**, each under its closest node ancestor | `tag` |
| `Class` | each class a stylesheet (or a page's `<style>`) defines | `rules`: described list of every rule the class appears in, **as written, in source order** (minified files skip it — one-line blobs are noise, not reading material) |
| `Const` | each `--custom-property` | `value` as written |
| `Function` | inline `<script>` functions (declarations and `const f = (…) =>`), **shallow-parsed** with the JS grammar at the C plugin's depth — an inline script is file-scoped by nature; module-world JS belongs to the ts plugin | `signature`, `_code` with include_source |
| stand-ins | CDN scripts/stylesheets — the URL is the identity | `File` + `External` |

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | page → element → nested element; stylesheet → class/property; page → inline function | declaration site |
| `IMPORTS` | page → stylesheet (`<link rel=stylesheet>`) / script file (`<script src>`); stylesheet → `@import` target; CDN URLs → external File | the tag / `@import` |
| `LINKS` | page → page for internal `<a href>`; a `#fragment` lands on the id'd element itself when this run parsed it | the anchor |
| `STYLED_BY` | page or element → the class that styles it (from `class="…"`) | the attribute |
| `USES` | stylesheet → custom property (`var(--x)`) | the use |
| `CALLS` | inline function → inline function, within the page — an inline script's world is its page | call site |

## Resolution — nearest, then unique

- **Classes and custom properties** bind nearest-first: the page's own
  `<style>` before the stylesheets — matching how the cascade makes
  page-local rules feel local — then the **one unique definition**; a class
  two stylesheets define is counted, never guessed.
- With one carve-out the first real corpus demanded: **a `.min.css` is a
  build artifact of its readable sibling, not a second opinion** — minified
  definitions yield to source ones, and only tie-break among themselves
  when no source defines the name. (On sb-admin-2 this rule alone took
  `STYLED_BY` from zero — every class doubled by its min twin — to 1,771
  edges.)
- **Hrefs and imports** resolve against the parsed set (relative paths,
  `../`, absolute-from-root, `#fragment` tails; queries stripped); external
  URLs are counted as links to the world, CDN imports become external File
  nodes; references naming files the digest never saw (assets) are counted.
- Inline-script calls bind within the page's own functions; `fetch()` and
  friends are the platform's — counted.

Report notes: unresolved class/property/call references · external links ·
references to unseen files.

## Options (`[plugins.web]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to inline functions and custom properties |
| `dom = "full"` | **every** element becomes a node, keyed positionally (`p.html::html[1]>body[1]>div[2]`), nested all the way down; id'd elements keep their stable `page#id` form. Opt-in because positional keys are deterministic within a snapshot but churn when markup shifts |

## Build & test

```console
$ cd parser && cargo test             # 16 tests
$ just web-plugin                     # needs wasi-sdk
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_web.wasm
```

## Known limits

SCSS/LESS compile *to* CSS and have grammars of their own — deliberately
not claimed in v1; framework templates (Vue/Svelte SFCs) are not html;
selector matching beyond class-name identity (descendant combinators,
specificity) is a browser's business, not a parser's.
