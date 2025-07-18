export default {
  "*.{rs,toml}": () => [
    "cargo +nightly fmt --check -- --color always",
    "cargo clippy --locked --color always -- -D warnings",
  ],
  "*.proto": () => [
    "cd proto && buf lint && buf format --exit-code > /dev/null",
  ],
  "*.py": ["ruff format --check", "ruff check"],
  "!(*test*)*": "typos --config .typos.toml",
};
