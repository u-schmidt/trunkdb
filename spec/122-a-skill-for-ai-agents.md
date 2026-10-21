# 122. A skill for AI agents (`skills/trunkdb/`)

An AI agent with a shell can use the `trunkdb` command as a person
does, and `--help` tells it the commands. It doesn't tell it the order
that keeps its context small, the shape of the filter language, or what
not to do, so it learns those by trying. Now the repository carries an
Agent Skill that says them: a folder in the open format Claude Code,
Codex, Gemini CLI and others read, a `SKILL.md` with a name and a
description that say when it applies, and instructions the agent loads
only then.

```text
skills/trunkdb/
├── SKILL.md            when it applies; the workflow; reading the output; what not to do
└── references/
    └── filters.md      the filter language in full, read when a query needs it
```

## 122.1 What it says
- **Outside in:** `info`, then `fields` (§121), then `find` with
  `select` and `limit`; `--explain` for how a query ran.
- **Counts without the documents:** `find … > /dev/null` keeps only
  `found N documents`; counts per group by selecting the one field and
  counting in the pipe, `jq | sort | uniq -c`, so only the counts reach
  the agent.
- **The common filters** on one screen, and the shell's quoting: the
  filter in single quotes, its strings in double ones, a date with its
  time.
- **The tags** of what it prints, `$id`, `$date`, `$binary`.
- **What not to do:** `export` to answer a question, `compact` or
  `import` unasked, getting around a lock by copying the file, which
  can miss what is in its WAL. Changing documents is said to be beyond
  the command for now.

`references/filters.md` has the rest of §111, §115, §32 and §42 as an
agent needs it: names in backticks, the reserved words, how null and
missing compare, `[*]` and `has`, the order of kinds in a sort, what
the common mistakes look like.

## 122.2 Where it lives
In the repository, beside the command it describes, so a change to the
command and to its skill is one commit. Excluded from the crate: it's
for agents, not for a build. The README says how to install it: the
folder copied to where an agent reads skills.

## 122.3 Rejected
- **A skill per command**, a query skill and later an update skill. An
  agent picks a skill by its description, and "look into a trunkdb
  file" is one task: several near-alike descriptions would compete. And
  the order of the commands, which is most of what the skill says,
  would belong to none of them.
- **An MCP server first.** A program of its own, with a dependency, its
  own tests and releases, to do what the command already does for an
  agent with a shell. It's for agents without one, later, as a thin
  layer over the same functions.
- **Everything in `SKILL.md`.** The filter language in full would be
  read on every use; most questions need only the common forms.
- **Restricting writes in the skill.** A skill is advice; what an agent
  may run is its permission settings' to enforce.

## 122.4 Tried
No test can say whether instructions help, so they were tried: a file
of 20,000 tasks and 4 people, with optional fields, a `priority` that
is a number in most documents and a string in some, and tags stored
both missing and as `[]`. Two questions, each answered by a fresh agent
given only the command, and by one given the skill as well:
- **"What's in it, which fields can be missing, is anything
  inconsistent?"** Both found the collections, the three optional
  fields with their counts, and the mixed `priority`. With the skill:
  8 calls, 58 s, no mistakes. Without: 10 calls, 66 s, three filters
  refused (single quotes, a date without its time), and the whole
  database exported to analyse it in a script.
- **"Open tasks per team, and how many are overdue?"** Both answered
  every number right. With the skill: 7 calls, 32 s, no mistakes.
  Without: 9 calls, 47 s, a date written bare and refused, every
  document piped into a script.

Tokens were the same within 2%: what the skill costs to read, about
100 lines, is what `--help` and the mistakes cost without it. The
gains are fewer wrong turns and no export, which on a larger file is
where the difference grows. What the runs showed went back into the
skill: the quoting and the date's time spelled out, and the count kept
out of `jq` (`2>/dev/null`), where one run had fed it in.

## 122.5 Limits
- Reading only, as the command is. When `delete`, `update` and
  `insert` come, the skill learns them in the same commits.
- Kept true by hand: nothing checks it against the command. A command
  added or changed is a reason to read it again.
