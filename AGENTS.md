# AGENTS.md

Project instructions for agents working in this repository. Read these first.

## Standing user rules

- **Full commit authority.** You have full control of the codebase and are
  authorized to commit **whenever it is needed** — do not stop to ask for commit
  permission. Commit after every milestone slice; push only when the current
  tasklist is finished.
- **Commit messages:** plain conventional style, **no** milestone/jargon
  vocabulary (no "Milestone 2", "Task 5", "slice").
  Example: `feat(compiler,cli,docs): test suite describe groups and hooks`.
- **Repo hygiene:** `git status` + branch check before mutations; never
  `git add -A` / `git clean -fd` blindly; no nested `.git`.
- **Docs in the same change:** update `HANDOFF.md` (and `MEMORY.md`) alongside
  every docs update. Office working notes live in
  `C:\Users\vilni\Desktop\picklescript-office\`.

## Project shape (important)

- This Rust repo is the **bootstrap/oracle**. The self-hosted **port** lives in
  `pickle/**` (compiler, cli, runtime) — it is a **separate project** and is
  **gitignored here on purpose**; it will move to its **own GitHub repository**.
  Keep it ignored in this repo for now.
- **Source of truth is the code** (`compiler/src/*.rs`, `runtime/src/*.rs`,
  `pickle/**`, `stdlib/**`), **not** `docs/**` (stale/aspirational).
- Retire Rust 100% over time; performance matters. See
  `picklescript-office\ARCHITECTURE.md`.
- Repo-root grep skips gitignored `pickle/` — pass an explicit path when
  searching the port.
