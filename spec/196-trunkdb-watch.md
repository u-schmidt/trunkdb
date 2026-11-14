# 196. `trunkdb watch` (`bin/trunkdb.rs`)

The `trunkdb` command could look into a file the application has open
(§187), as it is when asked. With §195, a read-only open sees the
application's commits as they come; this puts that on the command line:
a terminal, a shell script or an agent following a collection while the
application runs.

```
$ trunkdb watch app.trunkdb vans 'zone = "depot"'
{"event":"entered","id":"01a1…","doc":{"_id":{"$id":"01a1…"},"name":"a","zone":"depot"},"at":null}
{"event":"entered","id":"01a2…","doc":{…},"at":null}
{"event":"left","id":"01a1…","doc":{…,"zone":"road"},"from":null}
```

## 196.1 The command
- **`trunkdb watch <file> <collection> [filter]`:** a live query (§138,
  §145), the filter's words joined as `find` joins them; with a sort,
  the places in the list.
- **`… <filter> --aggregate <aggregate>`:** a live aggregate (§139), the
  aggregate as `trunkdb aggregate` reads it (§157), its rows.
- **`--param name=value`** for either text (§163).
- **One JSON event a line,** as the C ABI hands them out (§194.2), so a
  program reads the command's and the library's alike: `entered`,
  `left` and `changed` with `id`, `doc`, `at` and `from`; or with
  `group` and `row`.
- **It runs until stopped:** Ctrl-C, or its reader gone, as `| head -3`
  goes, which ends it with exit code 0, as `find` does (§112). An error
  that ends the live query is printed, exit code 1.
- **It opens the file read-only** (§88), beside the application, or
  with none: then it reports what the next program that opens the file
  commits.

Refused as the library refuses, before any line: a filter that can't
be live (an `offset`, a `limit` without a sort, §145), a text that
doesn't read (exit code 2), a collection that isn't there, as `find`
refuses one, a typo finding nothing.

Each event is flushed as it comes, once none waits behind it: a pipe
gets the first state in one write and each look's events as they come.

## 196.2 Rejected
- **The library's events as text,** a `Display` for `Live` and
  `LiveRow`: a format to keep for every program, for one command. The
  command writes §194.2's itself, as the C ABI does; a test holds it to
  the lines.
- **`--every` for how often it looks:** 100 ms, `watch_every`'s
  default, is a screen's rate; a script that wants fewer reads looks
  less often by its own means.
- **A `watch` that prints the commits, `subscribe`'s:** beside a writer
  there are none to print (§195.2).

## 196.3 Tests
`tests/cli.rs`, the command run as a process beside a database the
test holds open:

- `watch_prints_what_the_program_with_the_file_open_commits`: a filter
  with a parameter; the first state, then entered, left as it is now,
  changed and left gone, each line compared whole;
- `watch_prints_places_and_rows`: a sorted list's places as a faster
  van pushes the first out; an aggregate's rows as a group comes in;
- `watch_refuses_what_it_cant_watch_and_ends_with_its_reader`: an
  `offset`, a collection that isn't there, an aggregate that doesn't
  read; and the reader gone after one line, the next ending the
  command with exit code 0.

## 196.4 Mutations
In §195.7's table, the four named "watch".
