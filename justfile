t:
    cargo t

fmt:
    rustfmt +nightly **/*.rs

clippy:
    cargo clippy --all-features -- -D warnings

enable-hooks:
    echo "#!/bin/sh\njust pre-commit" > .git/hooks/pre-commit

pre-commit: fmt clippy t

html-solver:
    cargo r -p html-solver
