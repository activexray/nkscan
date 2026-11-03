# Contributing to this project

All help is welcome and greatly appreciated!
If you would like to contribute to the project, we ask you adhere to the following policies:

## Before you open a PR

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

## Style

### Prose

Comments, doc comments, error messages, log lines and the changelog use Simplified Technical English ([ASD-STE100](https://www.asd-ste100.org/)) and American spelling.

- Write one idea in each sentence. Use the active voice and the simple present tense.
- Use a verb only in its plain meaning. Do not use a metaphor in place of a verb, such as "land", "sit", "carry", "walk" or "hold". Say what happens.
- Do not use em dashes. Use a period, a colon, or a new sentence.
- Keep the technical names from the specs, such as SET WINDOW, scanning pitch or multi-line read.

### Comments

- Write a comment in one line. Write a block comment only if one line cannot give the reason.
- Say why the code does something that is not clear from the code. Do not say what the code does, and do not say how the code was before.
- Do not put worked examples in library doc comments. Put concrete numbers in tests.

### Code

- Write the least code that gives the behavior. Use the standard library and idiomatic Rust.
- Use a new feature of the toolchain if it makes the code shorter or clearer.
- Use the types that exist before you add a new type. If a type is almost correct, change it.
- Do not branch on the scanner model. Decide from what the unit reports: its pages, its capabilities, or the data. Firmware behavior that the pages do not report goes in `Quirks` in `src/protocol/model.rs`, and nowhere else.
- Do not add bare constants. Give each constant a name and a comment that says where its value comes from.
- Put behavior that a Python caller needs in the library, not in the CLI.

### Commits

- Write the commit message in one line.

## AI Assistance

Using AI tools to help you contribute is nominally ok, we just want to remind you that *you* are asking the maintainers (human people) review your code and you need to be in the loop of that process.
As such, we ask that you use AI as a tool to help you write code, not as an agent that autonomously generates an entire contribution and submits it on your behalf.

A few simple expectations:

- **Understand your code.** You should be able to explain and answer questions about anything you submit.
- **Write the PR description in your own words.** A short, clear explanation of *your* change is far more useful than a pasted AI summary.
- **Test your change** before opening the PR.
- **Keep it focused.** A PR that claims to fix one thing but touches a lot of unrelated code is hard to review (broad prompts tend to cause this).
- **Disclose AI assistance** in the PR description, along with roughly how much was used (e.g. docs only vs. code generation). Trivial tab-completion of single keywords or short phrases doesn't need to be disclosed.
- **Do not credit an AI tool as an author or co-author of a commit.** Remove trailers such as `Co-Authored-By: Claude`. You are the author, and you are responsible for the code. The tool is not. Put the disclosure in the PR description instead.

Example disclosures:

> **AI Disclosure:** This PR was written primarily by Claude Code.
> **AI Disclosure:** I consulted ChatGPT to understand the codebase, but the solution was authored manually.
> **AI Disclosure:** None.

Disclosure isn't about discouraging AI use,it just helps reviewers know how much scrutiny a change needs, and it's a courtesy to the humans on the other end of the pull request.
