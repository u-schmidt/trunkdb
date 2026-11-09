namespace TrunkDb;

/// <summary>How a database is opened. Unset values are trunkdb's defaults.</summary>
public sealed record OpenOptions
{
    /// <summary>Open it for reading only; a missing file is an error.</summary>
    public bool ReadOnly { get; init; }

    /// <summary>Bytes of pages kept in memory.</summary>
    public ulong? CacheSize { get; init; }

    /// <summary>Bytes snapshots may hold of pages written since they were taken.</summary>
    public ulong? SnapshotMemory { get; init; }

    /// <summary>Pages in the log that start a checkpoint.</summary>
    public ulong? CheckpointPages { get; init; }

    /// <summary>Bytes of log that start a checkpoint.</summary>
    public ulong? CheckpointWalBytes { get; init; }

    /// <summary>
    /// How documents are read and written: these options, with trunkdb's
    /// converters added (<see cref="TrunkJson.Options"/>); the web's when null.
    /// </summary>
    public System.Text.Json.JsonSerializerOptions? Json { get; init; }
}

/// <summary>A database's check: <see cref="Ok"/> when <see cref="Problems"/> is empty.</summary>
public sealed record CheckReport(bool Ok, ulong Collections, ulong Documents, ulong Pages, IReadOnlyList<string> Problems);

/// <summary>A compaction: the file's pages before and after.</summary>
public sealed record CompactReport(ulong PagesBefore, ulong PagesAfter);

/// <summary>What an export wrote or an import read; <see cref="Ended"/> when it was whole.</summary>
public sealed record TransferSummary(ulong Collections, ulong Documents, bool Ended);

/// <summary>An index of a collection.</summary>
public sealed record IndexInfo(IReadOnlyList<string> Fields, bool Unique, bool Sparse);

/// <summary>An upsert: the document's id, and whether it was inserted rather than updated.</summary>
public readonly record struct Upserted(DocId Id, bool Inserted);

/// <summary>A batch: each op, and how many documents all of them wrote.</summary>
public sealed record BatchResult(IReadOnlyList<BatchOp> Ops, ulong Total, bool DryRun);

/// <summary>
/// One op of a batch: its line, what it is (<c>insert</c>, <c>update</c>,
/// <c>delete</c>, <c>upsert</c>, <c>require</c>, <c>require_none</c>), and
/// how many documents it wrote; an insert's or upsert's id, but not on a
/// dry run; an upsert's <see cref="Inserted"/>.
/// </summary>
public sealed record BatchOp(ulong Line, string Op, ulong Count, DocId? Id, bool? Inserted);

/// <summary>Where a database's pages go, and how big each collection is (SPEC §93).</summary>
public sealed record DatabaseStats(
    FileStats File,
    PageStats Pages,
    IReadOnlyList<CollectionStats> Collections,
    ulong WalBytes,
    RecoveredStats? Recovered);

/// <summary>The file: its format, its pages, and the releases that made and last wrote it.</summary>
public sealed record FileStats(
    ulong Format, ulong PageSize, ulong Pages, ulong FreePages, string? CreatedBy, string? WrittenBy);

/// <summary>The file's pages, by what they hold.</summary>
public sealed record PageStats(
    ulong Header, ulong Catalog, ulong Free, ulong Data, ulong Overflow, ulong Index, ulong Unaccounted);

/// <summary>A collection's sizes and indexes.</summary>
public sealed record CollectionStats(
    string Name,
    ulong Documents,
    ulong DataPages,
    ulong OverflowPages,
    ulong DocumentBytes,
    ulong FreeBytes,
    IndexStats Primary,
    IReadOnlyList<IndexStats> Indexes);

/// <summary>An index's sizes.</summary>
public sealed record IndexStats(
    IReadOnlyList<string> Fields, bool Unique, bool Sparse, ulong Entries, ulong Pages, ulong Depth, ulong KeyBytes);

/// <summary>What opening recovered from the log.</summary>
public sealed record RecoveredStats(ulong Pages, bool InMemory);
