---
name: nextsession
description: instructions what to do when resuming or starting a session
allowed-tools: Bash(git *), Bash(cargo test *), Bash(cargo build *), Bash(cargo clippy *), Bash(cargo fmt *), Bash(cargo run *), Bash(cargo doc *), Bash(cargo publish *), Bash(cargo install *), Bash(cargo uninstall *), Bash(cargo update *), Bash(cargo clean *), Bash(cargo new *), Bash(cargo init *), Bash(cargo check *), Bash(cargo search *)
---
read docs/devel/handovers/HANDOVER.md and follow the instructions. Ask me if you have any questions. Remember our general coding rules:
1. preference for pure functions in reusable modules over complex code
2. test driven development paradigm
3. understandable inline documentation even for junior contributors mandatory
4. Aiming at keeping code files under 500 lines of code wherever feasible; consider refactoring where possible when files getting too large
5. Avoid technical debt - if you find an error, fix it when possible, else lodge it as issue on github
6. all tests must pass before committing, unless explicit permission is given by me
7. Before you start working, make sure that HANDOVER.md and ROADMAP.md represent the current state of progress and are up to date. If not, update them before you start working.
8. When you are done with your work, update HANDOVER.md and ROADMAP.md to reflect the current state of development and progress. If you are not sure how to do this, ask me.
