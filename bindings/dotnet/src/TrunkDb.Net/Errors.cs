using System.Text.Json;

namespace TrunkDb;

/// <summary>What went wrong, as trunkdb.h numbers it (SPEC §177.4).</summary>
public enum ErrorCode
{
    /// <summary>A handle, a pointer or a text that isn't what the call takes; or what the database refuses as asked, an index of other options.</summary>
    InvalidArgument = 1,
    /// <summary>A filter, document, update, parameters, aggregate or batch line that doesn't read.</summary>
    Parse = 2,
    /// <summary>The file system's error.</summary>
    Io = 3,
    /// <summary>A file that isn't trunkdb's, or of a format this release doesn't read.</summary>
    Format = 4,
    /// <summary>A write to a database opened read-only.</summary>
    ReadOnly = 5,
    /// <summary>A write failed half-way; the database must be opened again.</summary>
    Poisoned = 6,
    /// <summary>An insert of an id the collection has.</summary>
    DuplicateId = 7,
    /// <summary>A value a unique index has already.</summary>
    DuplicateValue = 8,
    /// <summary>A document that isn't there.</summary>
    NotFound = 9,
    /// <summary>More than one document where one was asked for.</summary>
    MultipleMatches = 10,
    /// <summary>An update that can't be applied to a document.</summary>
    Update = 11,
    /// <summary>A batch's precondition that failed: nothing was written.</summary>
    PreconditionFailed = 12,
    /// <summary>A document that isn't an object where one must be.</summary>
    NotAnObject = 13,
    /// <summary>A hint naming an index the collection hasn't.</summary>
    Hint = 14,
    /// <summary>An import's line that doesn't read.</summary>
    Import = 15,
    /// <summary>A compaction while a snapshot is open.</summary>
    SnapshotOpen = 16,
    /// <summary>A snapshot read after what it holds was given up.</summary>
    SnapshotTooOld = 17,
    /// <summary>trunkdb panicked; the process lives on.</summary>
    Panic = 18,
    /// <summary>A kind of error this release has no code of its own for.</summary>
    Other = 19,
}

/// <summary>
/// An error trunkdb returned: its <see cref="Code"/>, its message, and
/// <see cref="Detail"/>, every field the error has as JSON. A program
/// catches the subclasses by kind.
/// </summary>
public class TrunkDbException : Exception
{
    /// <summary>What kind of error it is.</summary>
    public ErrorCode Code { get; }

    /// <summary>The error as trunkdb gave it: <c>code</c>, <c>kind</c>, <c>message</c>, and the fields of its kind.</summary>
    public JsonElement Detail { get; }

    /// <summary>An error of <paramref name="code"/>, with its message and details.</summary>
    public TrunkDbException(ErrorCode code, string message, JsonElement detail) : base(message)
    {
        Code = code;
        Detail = detail;
    }

    /// <summary>A string of the details, or null.</summary>
    protected string? Text(string key) =>
        Detail.ValueKind == JsonValueKind.Object && Detail.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String
            ? v.GetString()
            : null;

    /// <summary>A number of the details, or null.</summary>
    protected long? Number(string key) =>
        Detail.ValueKind == JsonValueKind.Object && Detail.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.Number
            ? v.GetInt64()
            : null;

    /// <summary>An id of the details, or null.</summary>
    protected DocId? Id(string key) => Text(key) is { } text && DocId.TryParse(text, out var id) ? id : null;

    /// <summary>The calling thread's last error, read right after <paramref name="code"/> came back.</summary>
    internal static TrunkDbException Last(int code)
    {
        var message = Native.Read(Native.trunkdb_error_message());
        JsonElement detail;
        try
        {
            using var parsed = JsonDocument.Parse(Native.Read(Native.trunkdb_error_detail()));
            detail = parsed.RootElement.Clone();
        }
        catch (JsonException)
        {
            detail = default;
        }
        var kind = Enum.IsDefined(typeof(ErrorCode), code) ? (ErrorCode)code : ErrorCode.Other;
        return kind switch
        {
            ErrorCode.Parse => new ParseException(message, detail),
            ErrorCode.DuplicateId => new DuplicateIdException(message, detail),
            ErrorCode.DuplicateValue => new DuplicateValueException(message, detail),
            ErrorCode.NotFound => new NotFoundException(message, detail),
            ErrorCode.PreconditionFailed => new PreconditionFailedException(message, detail),
            ErrorCode.SnapshotTooOld => new SnapshotTooOldException(message, detail),
            _ => new TrunkDbException(kind, message, detail),
        };
    }
}

/// <summary>A text that doesn't read: a filter, document, update, parameters, aggregate or batch line.</summary>
public sealed class ParseException : TrunkDbException
{
    internal ParseException(string message, JsonElement detail) : base(ErrorCode.Parse, message, detail) { }

    /// <summary>Which text: <c>filter</c>, <c>document</c>, <c>update</c>, <c>params</c>, <c>aggregate</c>, <c>batch</c>…</summary>
    public string? What => Text("what");

    /// <summary>Where in the text, in bytes, if it is known.</summary>
    public long? Offset => Number("offset");

    /// <summary>A batch's line, from 1, if the text was a batch.</summary>
    public long? Line => Number("line");

    /// <summary>The parameter that was missing or not taken, if that was it.</summary>
    public string? Parameter => Text("parameter");
}

/// <summary>An insert of an id the collection has already.</summary>
public sealed class DuplicateIdException : TrunkDbException
{
    internal DuplicateIdException(string message, JsonElement detail) : base(ErrorCode.DuplicateId, message, detail) { }

    /// <summary>The collection.</summary>
    public string? Collection => Text("collection");

    /// <summary>The id it has.</summary>
    public DocId? DocumentId => Id("id");
}

/// <summary>A value a unique index has already: nothing was written.</summary>
public sealed class DuplicateValueException : TrunkDbException
{
    internal DuplicateValueException(string message, JsonElement detail) : base(ErrorCode.DuplicateValue, message, detail) { }

    /// <summary>The collection.</summary>
    public string? Collection => Text("collection");

    /// <summary>The unique index's field, or its fields joined with commas.</summary>
    public string? Field => Text("field");

    /// <summary>The document that was refused.</summary>
    public DocId? DocumentId => Id("id");

    /// <summary>The document that has the value.</summary>
    public DocId? Existing => Id("existing");
}

/// <summary>A document that isn't there.</summary>
public sealed class NotFoundException : TrunkDbException
{
    internal NotFoundException(string message, JsonElement detail) : base(ErrorCode.NotFound, message, detail) { }

    /// <summary>The collection.</summary>
    public string? Collection => Text("collection");

    /// <summary>The id looked for.</summary>
    public DocId? DocumentId => Id("id");
}

/// <summary>A batch's precondition that failed: nothing of the batch was written.</summary>
public sealed class PreconditionFailedException : TrunkDbException
{
    internal PreconditionFailedException(string message, JsonElement detail) : base(ErrorCode.PreconditionFailed, message, detail) { }

    /// <summary>The collection checked.</summary>
    public string? Collection => Text("collection");

    /// <summary>The precondition's place among the batch's ops, from 0.</summary>
    public long? Op => Number("op");

    /// <summary>Its line in the batch's text, from 1.</summary>
    public long? Line => Number("line");

    /// <summary>Whether a document matched: true for a <c>require_none</c>, false for a <c>require</c>.</summary>
    public bool? Matched =>
        Detail.ValueKind == JsonValueKind.Object && Detail.TryGetProperty("matched", out var v)
            && v.ValueKind is JsonValueKind.True or JsonValueKind.False
            ? v.GetBoolean()
            : null;
}

/// <summary>A snapshot read after what it holds was given up; take a new snapshot.</summary>
public sealed class SnapshotTooOldException : TrunkDbException
{
    internal SnapshotTooOldException(string message, JsonElement detail) : base(ErrorCode.SnapshotTooOld, message, detail) { }
}
