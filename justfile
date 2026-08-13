# Refresh every vendored copy of the canonical contract. Run after editing
# `wit/preprocess.wit`; CI fails if a copy has drifted.
vendor-wit:
    cp wit/preprocess.wit sdk/rust/wit/preprocess.wit

# What CI runs: no copy may differ from the canonical file.
check-wit:
    diff -u wit/preprocess.wit sdk/rust/wit/preprocess.wit
