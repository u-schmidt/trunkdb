# The aggregate language

What `trunkdb aggregate <file> <collection> '<filter>' '<aggregate>'`
takes as its last argument: SQL's select list, `GROUP BY` and what
follows it. The filter picks the documents first, as for `find`; the
aggregate puts them in groups and sums each group up into one row.

```text
count() as n, avg(hours) as mean, max(due) as latest
group by team, status
having n > 5
order by n desc, team
limit 10
```

Each row holds the group's values at their paths, then each column
under its name, and no `_id`. Without `group by`, there is one row of
every document found, even of none.

## Clauses

In this order, each one optional:

| Clause | What |
|---|---|
| `window <path> <duration>` | only the documents whose date at `<path>` is within the duration of the newest one's: `window at 5m` |
| `unwind <path>, …` | each document once per element of the array at `<path>`, the element in its place, MongoDB's `$unwind` |
| columns | `<function> as <name>`, between commas |
| `group by <path>, …` | one row per different value (or values) |
| `having <condition>` | keeps the rows a condition on columns and group values holds for, as a filter's |
| `order by <name> [desc], …` | the rows' order; by group values, ascending, without it |
| `limit <n>`, `offset <n>` | as for a filter |

A duration is a whole number and a unit: `ns`, `us`, `ms`, `s`, `m`,
`h`, `d` (`90s`, `5m`, `2d`). A window counts back from the newest
document, not the clock.

## Columns

| Column | What, per group |
|---|---|
| `count()` or `count(*)` | how many documents; it takes no path: for those with a value there, `count() filter (where exists(p))` |
| `count(distinct <path>)` | how many different values; null and missing not counted |
| `sum(<path>)` | the numbers' total; others passed over; `0` for none |
| `avg(<path>)` | the numbers' mean; `null` for none |
| `min(<path>)`, `max(<path>)` | in sort order; null and missing passed over |
| `first(<path>)`, `last(<path>)` | the value in the first or last document by id, the order they were made in |
| `first(<path> order by <path>)` | by another field: `last(pos order by at)`, each van's newest position |

Any column takes `filter (where <condition>)` before its `as`, to take
in only some documents, SQL's `FILTER`: `count() filter (where status
= "open") as open`. The other columns, and the groups, still see every
document the filter found.

## Examples

```text
count() as n group by status                                  -- per status
sum(amount) as total, count() as orders group by customer
  order by total desc limit 10                                -- top customers
unwind items sum(items.qty) as sold group by items.sku        -- per element
count() as n, count() filter (where late = true) as late
  group by team having late > 0                               -- teams with late tasks
window at 5m avg(load) as mean group by server                -- the last five minutes
last(status order by changed) as now group by ticket          -- each one's newest
```

Words here (`group`, `having`, `as`, `filter`, `distinct`, `unwind`,
`window`) are keywords only where the aggregate expects them; a field
with such a name is written in backticks if it would be read as one.
A mistake prints the aggregate with a `^` under it, exit code 2.
