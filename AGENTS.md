# AGENTS.md

- Keep it as simple as possible. Cut low-value features and speculative abstractions, simplify or delete before adding.
- Prioritize speed and footprint over features and polish. Measure startup time, click-to-first-token latency, memory use, and release size where relevant.
- Reuse lean, proven crates on current stable releases. Disable unused Cargo features, strip release binaries, and avoid deprecated protocols.
- Use a minimal-footprint architecture: cohesive modules, narrow interfaces, and a minimal `main.rs`. Make features easy to add, change, or remove without touching unrelated code.
- Follow idiomatic Rust and established naming and formatting conventions. Write in English. Comment only when the code cannot make an essential constraint clear.
- Treat configuration as code: keep app settings in readable, reproducible, hot-reloaded `config.toml`, not the UI. Keep `config.example.toml` in sync with the schema and global secrets in the environment.
- Keep the UI dark, server-rendered, and chat-only. Ship minimal HTML/CSS/JS; no animations, settings pages, or service worker.
- Do not work on `main` branch. Commit per milestone and stop for review. Follow conventionalcommits.org. Write docs only on request. The maintainer signs, pushes, and merges.
