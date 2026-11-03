# Contributing to this project

All help is welcome and greatly appreciated!
If you would like to contribute to the project, we ask you adhere to the following policies:

## Before you open a PR

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

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
