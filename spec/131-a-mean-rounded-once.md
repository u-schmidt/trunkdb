# 131. A mean rounded once (`aggregate.rs`)

A fix to §107. 107.2 says an `avg` is "that exact total divided by how
many numbers it holds". The total is exact: a fixed-point integer wide
enough for any sum of floats (107.4). But `ExactSum::avg` first rounded
that total to a float, then divided the float by the count. That is two
roundings, and the first can overflow.

A robustness review found it (2026-10-26):
`Aggregate::new().avg("n", "mean")` over two documents of
`{"n": 1e308}` gave `{"mean": Infinity}`. The total, 2e308, is past
`f64::MAX`; the mean, 1e308, is not. Every plan gave the same wrong
answer, scan or index, since they share `ExactSum`. That is how the
differential tests (§105) missed it.

The double rounding was also wrong where nothing overflows. When the
total isn't a float itself, rounding it and then rounding the quotient
can land a unit in the last place away from the mean, rounded once.
A short random search over means of three to seven floats, checked
against Python's `Fraction`, found three such cases.

## 131.1 The fix
- **`Wide::div_to_f64(n)`:** the wide integer divided by the count, a
  64-bit limb at a time from the top, with the remainder kept below `n`.
  Then rounded once, to nearest, ties to even, as `to_f64` rounds a
  total. The remainder counts as bits past the last: past half a unit,
  exactly half, or less.
- **One rounding for both.** `to_f64` and `div_to_f64` both go through
  `rounded`. A total is a division that left nothing.
- **Below the smallest normal float** a mean is a whole number of the
  smallest units, plus the remainder. Past half a unit rounds up, and
  exactly half rounds to the even unit.
- **Never `-0.0`,** as for a sum (107.2). A tiny negative mean that
  rounds to zero is `0.0`. Before, `-5e-324 / 2` was `-0.0`.
- **Infinities and NaN** are divided as before, as IEEE divides them.

**Changed answers.** A mean of finite numbers whose total is past
`f64::MAX` is now finite. Elsewhere a mean can differ from before by one
unit in the last place: the mean rounded once instead of twice. Means
of integers below 2^53, and of floats whose total is exact, don't
change. `sum`, `count`, `min` and `max` don't change.

## 131.2 Rejected
- **The quotient only where the total overflows:** it would fix the
  review's case and keep the double rounding everywhere else. The spec
  promises the exact total divided.
- **Dividing every number by the count as it is added:** that rounds
  once per number, and the mean would depend on the order the
  documents come in, which 107.4 rules out.
- **A total scaled down before rounding (`total / 2^k`, then times
  `2^k / n`):** two roundings again, and more steps.

## 131.3 Tests
- `means_are_the_exact_total_divided_and_rounded_once` (`aggregate.rs`):
  - Totals past `f64::MAX`: two `1e308`, three `f64::MAX`, two
    `-f64::MAX`; and with cancellation, `max, -max, max` is `max / 3`.
  - Below the smallest normal float: half the smallest unit, a tie, is
    `0.0`; one and a half units round to two; a mean that rounds to
    zero from below is `0.0`, not `-0.0`.
  - Just above them, a quotient whose last bit is the round bit and
    whose remainder is a third of a unit: it rounds up, where the bits
    alone say a tie.
  - The three random cases where the old way was a unit off, as
    `Fraction` gives them.
  - 300 random means of a power of two of floats, from 1 to 32 of
    them: dividing by a power of two is exact, so the mean is the
    rounded total divided, to the bit.
  - Infinities and NaN, and the review's case through `Aggregate`.
- `a_mean_of_large_numbers_is_finite_however_it_is_read`
  (`collection.rs`): three large floats, by scan and through an index
  on the field. One finite mean, within a unit of the expected one.
  Through the index the documents are read: an entry gives `avg` a
  number only below 2^53 (107.5).

The check against Python's `Fraction` was made once, and its answers
are in the test as bit patterns. The independent oracle in the test
itself is the power-of-two count.

## 131.4 Mutations
| Mutation | Caught by |
|---|---|
| The total rounded, then divided: the code before | both new tests |
| The remainder left out of the sticky bit | the case just above the subnormals |
| A tie below the normals rounding up always | the one and a half units |
| The remainder compared to `n` instead of `n / 2` | the subnormal cases |
| `-0.0` let through | the tiny negative mean |

## 131.5 Limits
- A division costs 34 limbs of `u128` arithmetic per group. A total
  costs that already, so `avg` is about twice what it was at the end,
  and nothing per document.
