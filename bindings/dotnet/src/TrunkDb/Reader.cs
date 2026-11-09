using System.Text.Json;

namespace TrunkDb;

/// <summary>
/// The reads, of a <see cref="Database"/> as it is now or of a
/// <see cref="Snapshot"/> as it was taken. A filter is trunkdb's text,
/// <c>status = :s order by due limit 10</c>, null for every document; its
/// parameters' values an object, <c>new { s = "open" }</c>, written with
/// <see cref="JsonOptions"/> as documents are.
/// </summary>
public abstract class Reader
{
    private readonly JsonSerializerOptions parameters;

    private protected Reader(JsonSerializerOptions json, JsonSerializerOptions parameters)
    {
        JsonOptions = json;
        this.parameters = parameters;
    }

    /// <summary>How documents are read and written: <see cref="OpenOptions.Json"/> with trunkdb's converters.</summary>
    public JsonSerializerOptions JsonOptions { get; }

    private protected abstract DatabaseHandle Db { get; }

    private protected abstract SnapshotHandle At { get; }

    /// <summary>The document with <paramref name="id"/>, or null.</summary>
    public T? Get<T>(string collection, DocId id)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_get(Db, At, collection, id.ToString(), out var document));
        return Read<T>(Native.Take(document));
    }

    /// <summary>The documents <paramref name="filter"/> matches, in its order.</summary>
    public List<T> Find<T>(string collection, string? filter = null, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_find(Db, At, collection, filter, Params(parameters), out var documents));
        return JsonSerializer.Deserialize<List<T>>(Native.TakeText(documents), JsonOptions)!;
    }

    /// <summary>The first document <paramref name="filter"/> matches, or null.</summary>
    public T? FindOne<T>(string collection, string? filter = null, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_find_one(Db, At, collection, filter, Params(parameters), out var document));
        return Read<T>(Native.Take(document));
    }

    /// <summary>How many documents <paramref name="filter"/> matches; 0 for a collection not there.</summary>
    public ulong Count(string collection, string? filter = null, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_count(Db, At, collection, filter, Params(parameters), out var count));
        return count;
    }

    /// <summary>
    /// The rows of <paramref name="aggregate"/>, <c>group by status: count() as n</c>,
    /// over the documents <paramref name="filter"/> matches. The values
    /// are for both texts' parameters.
    /// </summary>
    public List<T> Aggregate<T>(string collection, string? filter, string aggregate, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        ArgumentNullException.ThrowIfNull(aggregate);
        Native.Check(Native.trunkdb_aggregate(Db, At, collection, filter, aggregate, Params(parameters), out var rows));
        return JsonSerializer.Deserialize<List<T>>(Native.TakeText(rows), JsonOptions)!;
    }

    /// <summary>How <paramref name="filter"/> would be read: its plan, as <c>trunkdb find --explain</c> prints it.</summary>
    public string Explain(string collection, string? filter = null, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_explain(Db, At, collection, filter, Params(parameters), out var explained));
        return Native.TakeText(explained);
    }

    /// <summary>
    /// The documents <paramref name="filter"/> matches, one at a time, as
    /// they are when it opens: for more than fit in memory at once. Read
    /// once; a <c>foreach</c> to its end closes it, and so does
    /// <see cref="Cursor{T}.Dispose"/>.
    /// </summary>
    public Cursor<T> OpenCursor<T>(string collection, string? filter = null, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_cursor_open(Db, At, collection, filter, Params(parameters), out var cursor));
        return new Cursor<T>(cursor, JsonOptions);
    }

    /// <summary>Values for a text's parameters, as JSON; null for none.</summary>
    private protected string? Params(object? parameters) =>
        parameters is null ? null : JsonSerializer.Serialize(parameters, parameters.GetType(), this.parameters);

    private T? Read<T>(string? json) => json is null ? default : JsonSerializer.Deserialize<T>(json, JsonOptions);
}

/// <summary>
/// The database as it was when the snapshot was taken (SPEC §77): every
/// read of it sees that, whatever is written since. Dispose it when done:
/// an open snapshot holds what was overwritten since, and a compaction
/// waits for it. Its database must stay open while it is read.
/// </summary>
public sealed class Snapshot : Reader, IDisposable
{
    private readonly Database database;
    private readonly SnapshotHandle handle;

    internal Snapshot(Database database, SnapshotHandle handle) : base(database.JsonOptions, database.ParamsOptions)
    {
        this.database = database;
        this.handle = handle;
    }

    private protected override DatabaseHandle Db => database.Handle;

    private protected override SnapshotHandle At => handle;

    /// <summary>Closes the snapshot.</summary>
    public void Dispose() => handle.Dispose();
}

/// <summary>
/// Documents read one at a time (SPEC §80), as they were when the cursor
/// opened. Read once; disposed at the end of a <c>foreach</c>, or by
/// <see cref="Dispose"/> when left before it.
/// </summary>
public sealed class Cursor<T> : IEnumerable<T>, IDisposable
{
    private readonly CursorHandle handle;
    private readonly JsonSerializerOptions json;
    private bool started;

    internal Cursor(CursorHandle handle, JsonSerializerOptions json)
    {
        this.handle = handle;
        this.json = json;
    }

    /// <summary>The documents; a second enumeration is refused.</summary>
    /// <exception cref="InvalidOperationException">The cursor was read already.</exception>
    public IEnumerator<T> GetEnumerator()
    {
        if (started)
        {
            throw new InvalidOperationException("a cursor is read once: open another");
        }
        started = true;
        return Documents();
    }

    System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();

    private IEnumerator<T> Documents()
    {
        try
        {
            while (true)
            {
                Native.Check(Native.trunkdb_cursor_next(handle, out var document));
                if (Native.Take(document) is not { } text)
                {
                    yield break;
                }
                yield return JsonSerializer.Deserialize<T>(text, json)!;
            }
        }
        finally
        {
            handle.Dispose();
        }
    }

    /// <summary>Closes the cursor.</summary>
    public void Dispose() => handle.Dispose();
}
