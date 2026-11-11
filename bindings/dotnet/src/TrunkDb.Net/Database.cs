using System.Text.Json;
using System.Text.Json.Nodes;

namespace TrunkDb;

/// <summary>
/// A trunkdb database file (SPEC §180). Open it once and share it: every
/// method, and every collection's, may be called from any number of
/// threads at once. Dispose it to close the file.
/// <code>
/// using var db = Database.Open("app.trunkdb");
/// var tasks = db.Collection&lt;TaskItem&gt;("tasks");
/// var task = new TaskItem { Title = "Write it" };
/// tasks.Insert(task);
/// task.Status = "done";
/// tasks.Update(task);
/// foreach (var open in tasks.Find("status = :s order by due limit 10", new { s = "open" }))
///     Console.WriteLine(open.Title);
/// </code>
/// </summary>
public sealed class Database : IDisposable
{
    private readonly DatabaseHandle handle;
    private readonly JsonSerializerOptions parameters;

    private Database(DatabaseHandle handle, JsonSerializerOptions json, JsonSerializerOptions parameters)
    {
        this.handle = handle;
        JsonOptions = json;
        this.parameters = parameters;
    }

    internal DatabaseHandle Handle => handle;

    /// <summary>How documents are read and written: <see cref="OpenOptions.Json"/> with trunkdb's converters.</summary>
    public JsonSerializerOptions JsonOptions { get; }

    /// <summary>The release of the trunkdb library loaded, <c>0.39.0</c>.</summary>
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

    /// <summary>
    /// The collection <paramref name="name"/>, its documents as
    /// <typeparamref name="T"/>: a class of the program, or a
    /// <see cref="JsonObject"/> for documents as trunkdb's JSON.
    /// </summary>
    public Collection<T> Collection<T>(string name) => new(this, name);

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

    /// <summary>A document as JSON.</summary>
    internal string Write<T>(T document)
    {
        ArgumentNullException.ThrowIfNull(document);
        return JsonSerializer.Serialize(document, JsonOptions);
    }

    /// <summary>A document from JSON; null for none.</summary>
    internal T? Read<T>(string? json) => json is null ? default : JsonSerializer.Deserialize<T>(json, JsonOptions);

    /// <summary>Values for a text's parameters, as JSON; null for none.</summary>
    internal string? Params(object? values) =>
        values is null ? null : JsonSerializer.Serialize(values, values.GetType(), parameters);

    internal static T Report<T>(IntPtr text) =>
        JsonSerializer.Deserialize<T>(Native.TakeText(text), TrunkJson.Reports)!;
}
