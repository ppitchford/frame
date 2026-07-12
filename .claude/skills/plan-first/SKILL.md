---
name: plan-first
description: Structured planning workflow for any coding task. Use at the start of every new feature, bug fix, refactor, or implementation request — even small ones. Analyzes the project, asks targeted clarifying questions, produces a TODO.md, gets user approval, then executes task by task. No code is written before a plan is approved.
---

# Plan-First Workflow

## Rules

These are absolute. Each is restated in context within the phases below; this section is the quick reference.

- Never write code, create files, or run commands before `TODO.md` is approved.
- Never assume missing information. Ask. If too many unknowns exist to resolve in a single round of questions, surface that fact and ask the user to narrow scope before planning.
- Never skip phases. Follow them in order.
- Never go off-plan. Work discovered mid-execution goes into `TODO.md` under a `Discovered Tasks` section and waits for approval.

---

## Phase 1 — Analyze the Project

Before asking anything, read the project silently. Check:

1. Directory structure (top two levels).
2. Project manifest: `Cargo.toml`, `package.json`, `pubspec.yaml`, `go.mod`, `requirements.txt`, `pom.xml`, or equivalent.
3. Existing dependencies and their versions.
4. Build system and scripts (`Makefile`, `scripts/`, CI config).
5. `README.md` or equivalent.
6. Existing `TODO.md`, `TASKS.md`, `.todo`, or open-issue files.
7. Any `CLAUDE.md` or repo-specific Claude instructions.

Do not output analysis results unless they're directly relevant to a clarifying question.

**Greenfield exception:** if the project doesn't yet exist (empty directory, or only a `CLAUDE.md` / `README` placeholder), skip the analysis and proceed to Phase 2 to gather everything needed to scaffold it.

---

## Phase 2 — Ask Clarifying Questions (One Round)

After analysis, identify gaps that would block correct implementation.

- Ask only what is critical and cannot be inferred from the codebase.
- Aim for fewer than 5 questions. If more than 5 critical unknowns exist, surface that fact and ask the user to narrow scope before planning.
- Number the questions.
- Do not ask about things the project files already answer.
- Do not split into multiple rounds.

Example:

```
Before I create the plan, I need a few things clarified:

1. Should the new endpoint require authentication?
2. Is there a preferred database (the project has both SQLite and Postgres configs)?
3. Should existing tests be updated, or only new ones added?
```

Wait for the user's response before proceeding.

---

## Phase 3 — Create `TODO.md`

Using the analysis and the user's answers, write `TODO.md` in the project root.

### Structure

```
# TODO

## Goal
One sentence describing what will be built or fixed.

## Tasks

### 1. <Phase Name>
- [ ] <Concrete, measurable action>
- [ ] <Concrete, measurable action>

### 2. <Phase Name>
- [ ] <Concrete, measurable action>
- [ ] <Concrete, measurable action>

## Notes
Constraints, decisions, or known risks.
```

### Requirements

- Tasks are small and independently verifiable — one logical change each.
- Tasks are ordered by dependency. Prerequisites come first.
- Each task is checkable as done or not done. No vague items like "fix things" or "improve code."

After writing the file, show its contents to the user and ask:

```
I've created TODO.md. Does this plan look correct?
Reply YES to start, or tell me what to change.
```

---

## Phase 4 — Revision Loop

If the user requests changes:

1. Ask targeted follow-up questions if needed to resolve the disagreement.
2. Rewrite `TODO.md`.
3. Show the updated plan and ask for approval again.
4. Repeat until approved.

---

## Phase 5 — Execute the Plan

Once approved:

- Work through tasks in order, one at a time.
- State which task is starting before beginning it.
- After completing each task, mark it done in `TODO.md` by changing `- [ ]` to `- [x]`.
- Do not start the next task until the current one is complete.
- Do not perform any work not listed in `TODO.md`.

If unlisted work is discovered:

1. Stop.
2. Add the work to `TODO.md` under a `## Discovered Tasks` section.
3. Tell the user what was found and why it's needed.
4. Wait for approval before continuing.

When all tasks are marked `[x]`:

```
All tasks in TODO.md are complete.
```
