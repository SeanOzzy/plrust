#! /bin/bash

# requires:  "cargo install cargo-edit" from https://github.com/killercup/cargo-edit

cargo update

# generally speaking, the only pinned dependency we use is pgx, and generally speaking the only time we run this script
# is when we want to upgrade to a newer pgx.  `--pinned` has entered the chat
cargo upgrade --pinned
cargo generate-lockfile

