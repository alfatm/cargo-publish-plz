set export
set dotenv-load
set ignore-comments

PROFILE := env_var_or_default("PROFILE", "release")

lint:
    cargo fmt --all --check -- --unstable-features --error-on-unformatted
    cargo clippy --profile "$PROFILE" --all-features --all-targets
    cargo sort -c -w
    cargo machete

fix:
    cargo fmt --all -- --unstable-features --error-on-unformatted
    cargo clippy --fix --allow-dirty --allow-staged --all-features --all-targets
    cargo sort -w
    cargo machete --fix
