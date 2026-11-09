using System.Text.Json;
using System.Text.Json.Nodes;

namespace TrunkDb;

/// <summary>
/// A trunkdb database file (SPEC §180). Open it once and share it: every
/// method may be called from any number of threads at once. Dispose it
/// to close the file; snapshots and cursors still open keep it open
/// until they are disposed, though their reads need it.
/// <code>
/// using var db = Database.Open("app.trunkdb");
/// db.Insert("tasks", new TaskItem { Title = "Write it", Status = "open" });
/// foreach (var task in db.Find&lt;TaskItem&gt;("tasks", "status = :s limit 10", new { s = "open" }))
///     Console.WriteLine(task.Title);
/// </code>
/// Writes take documents as objects and updates as trunkdb's JSON
/// operators, <c>{"$inc": {"hours": 1}}</c>.
/// </summary>
public sealed class Database : Reader, IDisposable
{
    private readonly DatabaseHandle handle;

    private Database(DatabaseHandle handle, JsonSerializerOptions json, JsonSerializerOptions parameters)
        : base(json, parameters)
    {
        this.handle = handle;
        ParamsOptions = parameters;
    }

    internal JsonSerializerOptions ParamsOptions { get; }

    internal DatabaseHandle Handle => handle;

    private protected override DatabaseHandle Db => handle;

    private protected override SnapshotHandle At => SnapshotHandle.None;

    /// <summary>The release of the trunkdb library loaded, <c>0.36.0</c>.</summary>
    public static string LibraryVersion => Native.Read(Native.trunkdb_version());

    /// <summary>The version of the C ABI the library loaded speaks.</summary>
    public static uint AbiVersion => Native.trunkdb_abi_version();

    /// <summary>Opens the database at <paramref name="path"/>, creating it if it isn't there.</summary>
    /// <exception cref="TrunkDbException">The file can't be opened, or isn't trunkdb's.</exception>
    /// <exception cref="InvalidOperationException">The library loaded speaks another ABI than this wrapper.</exception>
    public static Database Open(string path, OpenOptions? options = null)
    {
        ArgumentNullException.ThrowIfNull(path);
        var abi = AbiVersion;
        if (abi != Native.AbiVersion)
        {
            throw new InvalidOperationException(
                $"the trunkdb library loaded, {LibraryVersion}, speaks ABI {abi}; this wrapper speaks {Native.AbiVersion}");
        }
        options ??= new OpenOptions();
        var given = new JsonObject();
        if (options.ReadOnly)
        {
            given["read_only"] = true;
        }
        Set(given, "cache_size", options.CacheSize);
        Set(given, "snapshot_memory", options.SnapshotMemory);
        Set(given, "checkpoint_pages", options.CheckpointPages);
        Set(given, "checkpoint_wal_bytes", options.CheckpointWalBytes);
        Native.Check(Native.trunkdb_open(path, given.ToJsonString(), out var db));
        return new Database(db, TrunkJson.Options(options.Json), TrunkJson.ParamsOptions(options.Json));
    }

    private static void Set(JsonObject options, string key, ulong? value)
    {
        if (value is { } n)
        {
            options[key] = n;
        }
    }

    /// <summary>Closes the database.</summary>
    public void Dispose() => handle.Dispose();

    // ---- Writes ----

    /// <summary>Inserts <paramref name="document"/>; its id, the one it has or a new one.</summary>
    /// <exception cref="DuplicateIdException">The collection has its id.</exception>
    /// <exception cref="DuplicateValueException">A unique index has one of its values.</exception>
    public DocId Insert<T>(string collection, T document)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_insert(handle, collection, Document(document), out var id));
        return DocId.Parse(Native.TakeText(id));
    }

    /// <summary>Replaces the document with <paramref name="id"/> whole; false if there is none.</summary>
    public bool Replace<T>(string collection, DocId id, T document)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_replace(handle, collection, id.ToString(), Document(document), out var found));
        return found;
    }

    /// <summary>Deletes the document with <paramref name="id"/>; false if there is none.</summary>
    public bool Delete(string collection, DocId id)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_delete(handle, collection, id.ToString(), out var found));
        return found;
    }

    /// <summary>
    /// Applies <paramref name="update"/>, JSON operators such as
    /// <c>{"$set": {"status": "done"}}</c>, to every document
    /// <paramref name="filter"/> matches, null for all; how many changed.
    /// </summary>
    public ulong Update(string collection, string? filter, string update, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        ArgumentNullException.ThrowIfNull(update);
        Native.Check(Native.trunkdb_update(handle, collection, filter, Params(parameters), update, out var count));
        return count;
    }

    /// <summary>Deletes every document <paramref name="filter"/> matches, null for all; how many.</summary>
    public ulong DeleteMany(string collection, string? filter, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_delete_many(handle, collection, filter, Params(parameters), out var count));
        return count;
    }

    /// <summary>
    /// Applies <paramref name="update"/> to the one document
    /// <paramref name="filter"/> matches, or inserts one made from the
    /// filter's equalities and the update.
    /// </summary>
    /// <exception cref="TrunkDbException">More than one document matches (<see cref="ErrorCode.MultipleMatches"/>).</exception>
    public Upserted Upsert(string collection, string filter, string update, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(collection);
        ArgumentNullException.ThrowIfNull(filter);
        ArgumentNullException.ThrowIfNull(update);
        Native.Check(Native.trunkdb_upsert(handle, collection, filter, Params(parameters), update, out var id, out var inserted));
        return new Upserted(DocId.Parse(Native.TakeText(id)), inserted);
    }

    /// <summary>
    /// Runs a batch, one op a line as <c>trunkdb batch</c> reads them, all
    /// committed or none (SPEC §168): <c>{"op": "insert", "collection":
    /// "tasks", "document": {…}}</c>. With <paramref name="dryRun"/>, each
    /// op is run and everything rolled back.
    /// </summary>
    /// <exception cref="PreconditionFailedException">A <c>require</c> or <c>require_none</c> failed; nothing was written.</exception>
    public BatchResult Batch(string lines, object? parameters = null, bool dryRun = false)
    {
        ArgumentNullException.ThrowIfNull(lines);
        Native.Check(Native.trunkdb_batch(handle, lines, Params(parameters), dryRun, out var written));
        return Report<BatchResult>(written);
    }

    // ---- Indexes ----

    /// <summary>Makes an index on <paramref name="field"/> if there is none; true if it made one.</summary>
    public bool EnsureIndex(string collection, string field, bool unique = false, bool sparse = false) =>
        EnsureIndex(collection, [field], unique, sparse);

    /// <summary>Makes an index on <paramref name="fields"/>, in their order, if there is none; true if it made one.</summary>
    /// <exception cref="TrunkDbException">There is one on them of other options (<see cref="ErrorCode.InvalidArgument"/>).</exception>
    /// <exception cref="DuplicateValueException">It is unique, and two documents have one value.</exception>
    public bool EnsureIndex(string collection, IReadOnlyList<string> fields, bool unique = false, bool sparse = false)
    {
        ArgumentNullException.ThrowIfNull(collection);
        ArgumentNullException.ThrowIfNull(fields);
        var options = new JsonObject { ["unique"] = unique, ["sparse"] = sparse };
        Native.Check(Native.trunkdb_index_ensure(handle, collection, Fields(fields), options.ToJsonString(), out var created));
        return created;
    }

    /// <summary>Drops the index on <paramref name="field"/>; false if there is none.</summary>
    public bool DropIndex(string collection, string field) => DropIndex(collection, [field]);

    /// <summary>Drops the index on <paramref name="fields"/>; false if there is none.</summary>
    public bool DropIndex(string collection, IReadOnlyList<string> fields)
    {
        ArgumentNullException.ThrowIfNull(collection);
        ArgumentNullException.ThrowIfNull(fields);
        Native.Check(Native.trunkdb_index_drop(handle, collection, Fields(fields), out var dropped));
        return dropped;
    }

    /// <summary>The indexes of <paramref name="collection"/>.</summary>
    public IReadOnlyList<IndexInfo> Indexes(string collection)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_indexes(handle, collection, out var indexes));
        return Report<List<IndexInfo>>(indexes);
    }

    private static string Fields(IReadOnlyList<string> fields) => JsonSerializer.Serialize(fields);

    // ---- The database ----

    /// <summary>The names of the collections.</summary>
    public IReadOnlyList<string> Collections()
    {
        Native.Check(Native.trunkdb_collections(handle, out var names));
        return Report<List<string>>(names);
    }

    /// <summary>Drops <paramref name="collection"/>, its documents and indexes; false if it isn't there.</summary>
    public bool DropCollection(string collection)
    {
        ArgumentNullException.ThrowIfNull(collection);
        Native.Check(Native.trunkdb_drop_collection(handle, collection, out var dropped));
        return dropped;
    }

    /// <summary>Takes a snapshot: reads of it see the database as it is now.</summary>
    public Snapshot OpenSnapshot()
    {
        Native.Check(Native.trunkdb_snapshot_open(handle, out var snapshot));
        return new Snapshot(this, snapshot);
    }

    /// <summary>Writes the log's pages into the file.</summary>
    public void Checkpoint() => Native.Check(Native.trunkdb_checkpoint(handle));

    /// <summary>Rewrites the file without its free pages.</summary>
    /// <exception cref="TrunkDbException">A snapshot holds the last commit (<see cref="ErrorCode.SnapshotOpen"/>).</exception>
    public CompactReport Compact()
    {
        Native.Check(Native.trunkdb_compact(handle, out var report));
        return Report<CompactReport>(report);
    }

    /// <summary>Reads every page and index and says what is wrong.</summary>
    public CheckReport Check()
    {
        Native.Check(Native.trunkdb_check(handle, out var report));
        return Report<CheckReport>(report);
    }

    /// <summary>Where the file's pages go, and each collection's sizes.</summary>
    public DatabaseStats Stats()
    {
        Native.Check(Native.trunkdb_stats(handle, out var report));
        return Report<DatabaseStats>(report);
    }

    /// <summary>Writes every collection to <paramref name="path"/>, one document a line, replacing the file once whole.</summary>
    public TransferSummary Export(string path)
    {
        ArgumentNullException.ThrowIfNull(path);
        Native.Check(Native.trunkdb_export(handle, path, out var summary));
        return Report<TransferSummary>(summary);
    }

    /// <summary>Reads an export at <paramref name="path"/> into this database, all of it or nothing.</summary>
    public TransferSummary Import(string path, ulong? maxLineBytes = null)
    {
        ArgumentNullException.ThrowIfNull(path);
        var options = new JsonObject();
        Set(options, "max_line_bytes", maxLineBytes);
        Native.Check(Native.trunkdb_import(handle, path, options.ToJsonString(), out var summary));
        return Report<TransferSummary>(summary);
    }

    private string Document<T>(T document)
    {
        ArgumentNullException.ThrowIfNull(document);
        return JsonSerializer.Serialize(document, JsonOptions);
    }

    private static T Report<T>(IntPtr text) =>
        JsonSerializer.Deserialize<T>(Native.TakeText(text), TrunkJson.Reports)!;
}
