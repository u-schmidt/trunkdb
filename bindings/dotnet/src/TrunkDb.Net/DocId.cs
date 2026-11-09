using System.Diagnostics.CodeAnalysis;

namespace TrunkDb;

/// <summary>
/// A document's id: a UUID, version 7 when trunkdb chose it, so ids
/// sort by when they were made. In a document it is <c>{"$id": "…"}</c>;
/// a property <c>Id</c> of this type is the document's <c>_id</c>.
/// <c>default</c> is <see cref="IsEmpty"/>: no id, trunkdb chooses one.
/// </summary>
public readonly struct DocId : IEquatable<DocId>, IComparable<DocId>
{
    private readonly Guid value;

    /// <summary>The id that is <paramref name="value"/>.</summary>
    public DocId(Guid value) => this.value = value;

    /// <summary>No id: what <c>default</c> is.</summary>
    public bool IsEmpty => value == Guid.Empty;

    /// <summary>The id as a <see cref="Guid"/>.</summary>
    public Guid ToGuid() => value;

    /// <summary>An id from its text, <c>01a1bc72-c792-772e-a864-a359a7ffaeb6</c>.</summary>
    /// <exception cref="FormatException">The text isn't a UUID.</exception>
    public static DocId Parse(string text) =>
        TryParse(text, out var id) ? id : throw new FormatException($"`{text}` is not an id: a UUID, like 01a1bc72-c792-772e-a864-a359a7ffaeb6");

    /// <summary>An id from its text, or false.</summary>
    public static bool TryParse([NotNullWhen(true)] string? text, out DocId id)
    {
        if (Guid.TryParseExact(text, "D", out var guid))
        {
            id = new DocId(guid);
            return true;
        }
        id = default;
        return false;
    }

    /// <summary>The id's text, as trunkdb writes it.</summary>
    public override string ToString() => value.ToString("D");

    /// <inheritdoc/>
    public bool Equals(DocId other) => value == other.value;

    /// <inheritdoc/>
    public override bool Equals(object? obj) => obj is DocId other && Equals(other);

    /// <inheritdoc/>
    public override int GetHashCode() => value.GetHashCode();

    /// <summary>In trunkdb's order, the order of the id's bytes, which is its text's.</summary>
    public int CompareTo(DocId other)
    {
        Span<byte> mine = stackalloc byte[16];
        Span<byte> theirs = stackalloc byte[16];
        value.TryWriteBytes(mine, bigEndian: true, out _);
        other.value.TryWriteBytes(theirs, bigEndian: true, out _);
        return mine.SequenceCompareTo(theirs);
    }

    /// <summary>Equal ids.</summary>
    public static bool operator ==(DocId left, DocId right) => left.Equals(right);

    /// <summary>Different ids.</summary>
    public static bool operator !=(DocId left, DocId right) => !left.Equals(right);

    /// <summary>Earlier in trunkdb's order.</summary>
    public static bool operator <(DocId left, DocId right) => left.CompareTo(right) < 0;

    /// <summary>Later in trunkdb's order.</summary>
    public static bool operator >(DocId left, DocId right) => left.CompareTo(right) > 0;

    /// <summary>Not later in trunkdb's order.</summary>
    public static bool operator <=(DocId left, DocId right) => left.CompareTo(right) <= 0;

    /// <summary>Not earlier in trunkdb's order.</summary>
    public static bool operator >=(DocId left, DocId right) => left.CompareTo(right) >= 0;
}
