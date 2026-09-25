# 1. Motivation

Background: an experienced C# developer, very little prior Rust experience.
On Windows, small personal projects used [LiteDB](https://www.litedb.org/)
— a single-file, embedded, document-oriented database (much closer to
MongoDB than to a relational DB: no tables, no joins, no schema versions).
LiteDB was mostly dormant for a while and has since become active again,
but it's .NET-only, and .NET is rarely used now.

Two motivations, not one:
1. Get back the *easy embedded-document-store* experience LiteDB provided,
   in a language actually used day to day.
2. Learn database internals — page storage, indexing, transactions,
   durability — which is currently a knowledge gap, not just a language gap.

Explicitly **not** a goal: translating or porting LiteDB's C# implementation
into Rust. LiteDB is a reference for calibrating scope and comparing
decisions against, not a source to copy from. Own design decisions,
including ones that turn out different from LiteDB's, are the point.
