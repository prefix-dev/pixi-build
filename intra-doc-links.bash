#!/bin/bash
cargo metadata --no-deps --format-version=1 \
| jq -r '.packages[] | .name as $pname | .targets[] | [$pname, .kind[], .name] | @tsv' \
| while IFS=$'\t' read -r package kind name; do
    case "$kind" in
        lib)
            cargo rustdoc -p "$package" --lib --all-features -- -D warnings -W unreachable-pub
            ;;
        bin)
            cargo rustdoc -p "$package" --bin "$name" --all-features -- -D warnings -W unreachable-pub
            ;;
        example)
            cargo rustdoc -p "$package" --example "$name" --all-features -- -D warnings -W unreachable-pub
            ;;
        test)
            cargo rustdoc -p "$package" --test "$name" --all-features -- -D warnings -W unreachable-pub
            ;;
        bench)
            cargo rustdoc -p "$package" --bench "$name" --all-features -- -D warnings -W unreachable-pub
            ;;
    esac
done
