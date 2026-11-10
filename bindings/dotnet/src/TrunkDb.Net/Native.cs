using System.Runtime.InteropServices;
using System.Runtime.InteropServices.Marshalling;
using Microsoft.Win32.SafeHandles;

namespace TrunkDb;

/// <summary>
/// The 36 functions of trunkdb.h, as C# calls them (SPEC §179). Text
/// going in is marshalled as UTF-8 for the call, by <see cref="NulFreeUtf8"/>,
/// which refuses a NUL in it (SPEC §184.1); text coming out is an
/// <see cref="IntPtr"/>, copied and freed at once by <see cref="Take"/>.
/// </summary>
internal static partial class Native
{
    private const string Library = "trunkdb";

    /// <summary>The ABI this wrapper is written against.</summary>
    internal const uint AbiVersion = 1;

    internal const int Ok = 0;

    [LibraryImport(Library)]
    internal static partial uint trunkdb_abi_version();

    [LibraryImport(Library)]
    internal static partial IntPtr trunkdb_version();

    [LibraryImport(Library)]
    internal static partial IntPtr trunkdb_error_message();

    [LibraryImport(Library)]
    internal static partial IntPtr trunkdb_error_detail();

    [LibraryImport(Library)]
    internal static partial void trunkdb_string_free(IntPtr text);

    // ---- The database ----

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_open(string path, string? options, out DatabaseHandle db);

    [LibraryImport(Library)]
    internal static partial int trunkdb_close(IntPtr db);

    [LibraryImport(Library)]
    internal static partial int trunkdb_checkpoint(DatabaseHandle db);

    [LibraryImport(Library)]
    internal static partial int trunkdb_compact(DatabaseHandle db, out IntPtr report);

    [LibraryImport(Library)]
    internal static partial int trunkdb_check(DatabaseHandle db, out IntPtr report);

    [LibraryImport(Library)]
    internal static partial int trunkdb_stats(DatabaseHandle db, out IntPtr report);

    [LibraryImport(Library)]
    internal static partial int trunkdb_collections(DatabaseHandle db, out IntPtr names);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_drop_collection(
        DatabaseHandle db, string collection, [MarshalAs(UnmanagedType.U1)] out bool dropped);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_export(DatabaseHandle db, string path, out IntPtr summary);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_import(
        DatabaseHandle db, string path, string? options, out IntPtr summary);

    // ---- Indexes ----

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_index_ensure(
        DatabaseHandle db, string collection, string fields, string? options,
        [MarshalAs(UnmanagedType.U1)] out bool created);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_index_drop(
        DatabaseHandle db, string collection, string fields,
        [MarshalAs(UnmanagedType.U1)] out bool dropped);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_indexes(DatabaseHandle db, string collection, out IntPtr indexes);

    // ---- Reads, of the database now or of a snapshot ----

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_get(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string id, out IntPtr document);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_find(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string? filter, string? parameters,
        out IntPtr documents);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_find_one(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string? filter, string? parameters,
        out IntPtr document);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_count(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string? filter, string? parameters,
        out ulong count);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_aggregate(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string? filter, string aggregate,
        string? parameters, out IntPtr rows);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_explain(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string? filter, string? parameters,
        out IntPtr explained);

    // ---- Writes ----

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_insert(DatabaseHandle db, string collection, string document, out IntPtr id);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_replace(
        DatabaseHandle db, string collection, string id, string document,
        [MarshalAs(UnmanagedType.U1)] out bool found);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_delete(
        DatabaseHandle db, string collection, string id, [MarshalAs(UnmanagedType.U1)] out bool found);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_update(
        DatabaseHandle db, string collection, string? filter, string? parameters, string update, out ulong count);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_delete_many(
        DatabaseHandle db, string collection, string? filter, string? parameters, out ulong count);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_upsert(
        DatabaseHandle db, string collection, string? filter, string? parameters, string update,
        out IntPtr id, [MarshalAs(UnmanagedType.U1)] out bool inserted);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_batch(
        DatabaseHandle db, string lines, string? parameters, [MarshalAs(UnmanagedType.U1)] bool dryRun,
        out IntPtr written);

    // ---- Snapshots and cursors ----

    [LibraryImport(Library)]
    internal static partial int trunkdb_snapshot_open(DatabaseHandle db, out SnapshotHandle snapshot);

    [LibraryImport(Library)]
    internal static partial int trunkdb_snapshot_close(IntPtr snapshot);

    [LibraryImport(Library, StringMarshalling = StringMarshalling.Custom, StringMarshallingCustomType = typeof(NulFreeUtf8))]
    internal static partial int trunkdb_cursor_open(
        DatabaseHandle db, SnapshotHandle snapshot, string collection, string? filter, string? parameters,
        out CursorHandle cursor);

    [LibraryImport(Library)]
    internal static partial int trunkdb_cursor_next(CursorHandle cursor, out IntPtr document);

    [LibraryImport(Library)]
    internal static partial int trunkdb_cursor_close(IntPtr cursor);

    // ---- Helpers ----

    /// <summary>Throws the calling thread's last error if <paramref name="code"/> isn't OK.</summary>
    internal static void Check(int code)
    {
        if (code != Ok)
        {
            throw TrunkDbException.Last(code);
        }
    }

    /// <summary>The text trunkdb handed out, copied, then given back; null for null.</summary>
    internal static string? Take(IntPtr text)
    {
        if (text == IntPtr.Zero)
        {
            return null;
        }
        try
        {
            return Marshal.PtrToStringUTF8(text);
        }
        finally
        {
            trunkdb_string_free(text);
        }
    }

    /// <summary>As <see cref="Take"/>, for text that is never null on success.</summary>
    internal static string TakeText(IntPtr text) =>
        Take(text) ?? throw new InvalidOperationException("trunkdb handed out no text where it promises some");

    /// <summary>Text trunkdb owns, copied: the version, the last error.</summary>
    internal static string Read(IntPtr text) => Marshal.PtrToStringUTF8(text) ?? "";
}

/// <summary>A database handle, closed by <c>trunkdb_close</c>.</summary>
internal sealed class DatabaseHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public DatabaseHandle() : base(true) { }

    protected override bool ReleaseHandle() => Native.trunkdb_close(handle) == Native.Ok;
}

/// <summary>
/// A snapshot handle, closed by <c>trunkdb_snapshot_close</c>.
/// <see cref="None"/>, never opened, passes null: a read of the database now.
/// </summary>
internal sealed class SnapshotHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public SnapshotHandle() : base(true) { }

    internal static readonly SnapshotHandle None = new();

    protected override bool ReleaseHandle() => Native.trunkdb_snapshot_close(handle) == Native.Ok;
}

/// <summary>A cursor handle, closed by <c>trunkdb_cursor_close</c>.</summary>
internal sealed class CursorHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public CursorHandle() : base(true) { }

    protected override bool ReleaseHandle() => Native.trunkdb_cursor_close(handle) == Native.Ok;
}

/// <summary>
/// Text going in, as UTF-8 with its NUL at the end, as
/// <see cref="Utf8StringMarshaller"/> makes it, but refused if it holds a
/// NUL of its own (SPEC §184.1). The library reads up to the first NUL, so
/// such text would reach it cut short: a filter without its last
/// conditions, another collection's name, another file's path.
/// </summary>
[CustomMarshaller(typeof(string), MarshalMode.ManagedToUnmanagedIn, typeof(NulFreeUtf8))]
internal static unsafe class NulFreeUtf8
{
    /// <exception cref="ArgumentException"><paramref name="managed"/> holds a NUL.</exception>
    public static byte* ConvertToUnmanaged(string? managed)
    {
        var at = managed?.IndexOf('\0') ?? -1;
        if (at >= 0)
        {
            throw new ArgumentException(
                $"text passed to trunkdb holds a NUL character (\\0) at {at}, where the library would end it: " +
                "it could name another collection or file, or lose a filter's conditions. " +
                "Remove it; a value from outside goes in through parameters, which keep it.");
        }
        return Utf8StringMarshaller.ConvertToUnmanaged(managed);
    }

    public static void Free(byte* unmanaged) => Utf8StringMarshaller.Free(unmanaged);
}
