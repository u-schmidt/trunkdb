# 148. Rows of the application's own (`collection.rs`, `live.rs`, `snapshot.rs`)

An aggregate's rows are `Document`s (§107), and a live aggregate's
`LiveRow` holds them (§139); §139.6 left typed rows for a step with a
typed `aggregate`. Until now a program made a struct of each row by
`Document::into_value`, row by row. This is that step.

```rust
#[derive(Deserialize)]
struct Load { server: String, mean: f64 }

let rows: Vec<Load> = readings.aggregate_as(Filter::new(), &per_server)?;
let live: LiveAggregate<Load> = readings.watch_aggregate_as(Filter::new(), &per_server)?;
```

## 148.1 The API
- **`Collection::aggregate_as::<R>(filter, &aggregate) -> Vec<R>`**,
  and on a snapshot's `View`: `aggregate`, each row converted to `R`, a
  `DeserializeOwned` of the application's own. A row that doesn't
  convert fails the call with `Error::Document`, as a typed `find`
  fails on a document that doesn't.
- **`Collection::watch_aggregate_as::<R>(filter, &aggregate) ->
  LiveAggregate<R>`**, whose events are `LiveRow<R>`: `row` an `R`,
  `Left`'s an `Option<R>`. `group` stays a `Document`: it is the key
  rows are known by, and a program that wants it typed has the row.
- **Converted on the reader's thread,** as a typed `LiveQuery<T>` is
  (§138.4), never under the writer's lock. A row that doesn't convert
  is an `Err` for that event, and the live aggregate goes on, the
  events behind it waiting.
- **`LiveRow<R = Document>` and `LiveAggregate<R = Document>`:** a
  default for the new parameter, so `LiveRow` and `LiveAggregate`
  written without one are what they were, and `watch_aggregate` is
  unchanged.

A row's struct has a field for each group field and each column, by
name; a dotted group field is nested as its path is (§107.2), so
`group_by("site.name")` wants a `site` field holding a `name`. A
column that can be `null`, `avg` over no numbers, `min` of nothing,
`first` of a missing value, is an `Option`.

## 148.2 Rows aren't the collection's `T`
A typed collection's `T` is its documents' shape; a row's is the
group values and the columns, which the aggregate makes, not the
collection. So the row type is named where the aggregate is run,
`aggregate_as::<R>`, on a typed collection as on one of `Document`,
as sqlx's `query_as::<_, Row>` names a query's.

## 148.3 Rejected
- **The row type in `Aggregate`,** `Aggregate<R>`: an aggregate is a
  value to build and keep, and the same one may be read as rows of
  several shapes, or as `Document`s for a tool.
- **`aggregate` itself generic,** `aggregate::<R>` with `Document` by
  default: a function's type parameter can't have a default, so every
  call that now infers `Vec<Document>` would need it said.
- **`group` typed too,** a second type parameter: the group's values
  are in the row already.
- **A row that doesn't convert ending the live aggregate:** the next
  commit may make it convert, and the aggregate on the writer's side
  is sound; a typed live query goes on likewise.

## 148.4 Tests
- `tests/live_aggregates.rs`:
  - `aggregate_as_converts_each_row`: rows as structs, `null` where no
    number was averaged an `Option`; a group field nested as its path,
    on a snapshot's view; a row that doesn't convert failing the call;
  - `a_live_aggregate_hands_out_rows_of_its_own`: `Entered`, `Changed`
    and `Left` with `R`s, `group` a document; a row leaving by the
    `having` with its converted row; a row that doesn't convert an
    `Err`, and the next event, waiting behind it, there after it.
- The doc examples of `aggregate_as` and `watch_aggregate_as`.
- Not in the `queries` fuzz target: the conversion is a layer over
  the rows it checks already, and a program has no struct to match
  rows it makes at random.

## 148.5 Mutations
| Mutation | Caught by |
|---|---|
| `Left` without its converted row | rows of its own, and three of §139's |
| `Changed` handed out as `Entered` | rows of its own, and nine of §139's |
| A row that doesn't convert ending it, the waiting events dropped | rows of its own |
| `aggregate_as` passing over a row that doesn't convert | converts each row |

One survived at first: a row that doesn't convert dropping the events
behind it went unseen while the next commit came after the error was
read. Both commits now come before either is read.
