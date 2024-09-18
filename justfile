t:
    cargo t

fmt:
    cargo +nightly fmt --all

clippy:
    cargo clippy --all-features -- -D warnings

enable-hooks:
    echo "#!/bin/sh\njust pre-commit" > .git/hooks/pre-commit

pre-commit: fmt clippy t

html-solver:
    cd utils/html-solver && \
    cargo r
