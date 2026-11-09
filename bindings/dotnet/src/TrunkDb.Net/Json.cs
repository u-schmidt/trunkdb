using System.Globalization;
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;

namespace TrunkDb;

/// <summary>
/// Documents through System.Text.Json, in the tagged JSON trunkdb reads
/// and writes (SPEC §30, §180): what plain JSON can't tell apart is a
/// one-key object.
/// <list type="table">
/// <item><term><see cref="DocId"/></term><description><c>{"$id": "01a1bc72-…"}</c>; a property <c>Id</c> of it is <c>_id</c></description></item>
/// <item><term><see cref="DateTime"/>, <see cref="DateTimeOffset"/></term><description><c>{"$date": "2026-11-09T14:05:00Z"}</c>, in UTC</description></item>
/// <item><term><see cref="DateOnly"/></term><description><c>{"$date": "2026-11-09"}</c></description></item>
/// <item><term><see cref="TimeOnly"/></term><description><c>{"$time": "14:05:00"}</c></description></item>
/// <item><term><c>byte[]</c></term><description><c>{"$binary": "&lt;base64&gt;"}</c></description></item>
/// <item><term><see cref="double"/>, <see cref="float"/></term><description><c>3.0</c>, never <c>3</c>, so it stays a float; NaN and infinities <c>{"$float": "NaN"}</c></description></item>
/// </list>
/// A <see cref="System.Text.Json.Nodes.JsonNode"/> or a <see cref="JsonElement"/> reads a document as it is, tags and all.
/// </summary>
public static class TrunkJson
{
    /// <summary>
    /// Options that read and write trunkdb's documents: <paramref name="from"/>'s,
    /// or the web's (camelCase names, read without regard to case), with the
    /// converters for the tags added.
    /// </summary>
    public static JsonSerializerOptions Options(JsonSerializerOptions? from = null) => Make(from, ids: true);

    /// <summary>
    /// As <see cref="Options"/>, for a text's parameters: no property is
    /// the document's <c>_id</c>, so <c>new { id }</c> is <c>:id</c>.
    /// </summary>
    internal static JsonSerializerOptions ParamsOptions(JsonSerializerOptions? from = null) => Make(from, ids: false);

    private static JsonSerializerOptions Make(JsonSerializerOptions? from, bool ids)
    {
        var options = from is null ? new JsonSerializerOptions(JsonSerializerDefaults.Web) : new JsonSerializerOptions(from);
        JsonConverter[] tags =
        [
            new DocIdConverter(),
            new DateTimeConverter(),
            new DateTimeOffsetConverter(),
            new DateOnlyConverter(),
            new TimeOnlyConverter(),
            new BinaryConverter(),
            new DoubleConverter(),
            new SingleConverter(),
        ];
        for (var i = 0; i < tags.Length; i++)
        {
            options.Converters.Insert(i, tags[i]);
        }
        if (ids)
        {
            var resolver = options.TypeInfoResolver ?? new DefaultJsonTypeInfoResolver();
            options.TypeInfoResolver = resolver.WithAddedModifier(MapIds);
        }
        return options;
    }

    /// <summary>The options a report is read with: its keys in snake_case, ids as text.</summary>
    internal static readonly JsonSerializerOptions Reports = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        Converters = { new DocIdTextConverter() },
    };

    /// <summary>
    /// A property <c>Id</c> of type <see cref="DocId"/> is the document's
    /// <c>_id</c>, whatever a naming policy would call it, unless it has a
    /// <see cref="JsonPropertyNameAttribute"/>; left out when empty, so
    /// trunkdb chooses one.
    /// </summary>
    private static void MapIds(JsonTypeInfo info)
    {
        if (info.Kind != JsonTypeInfoKind.Object)
        {
            return;
        }
        foreach (var property in info.Properties)
        {
            if (property.PropertyType != typeof(DocId) && property.PropertyType != typeof(DocId?))
            {
                continue;
            }
            // By the member's own name, whatever the naming policy made of
            // it; a name given with [JsonPropertyName] is kept.
            var member = property.AttributeProvider as MemberInfo;
            var named = member?.IsDefined(typeof(JsonPropertyNameAttribute), false) == true;
            if (!named && string.Equals(member?.Name ?? property.Name, "id", StringComparison.OrdinalIgnoreCase))
            {
                property.Name = "_id";
            }
            if (property.Name == "_id")
            {
                property.ShouldSerialize = static (_, value) => value is DocId id && !id.IsEmpty;
            }
        }
    }

    /// <summary>The text of <c>{"<paramref name="tag"/>": "…"}</c>, the reader left on its end.</summary>
    internal static string ReadTag(ref Utf8JsonReader reader, string tag)
    {
        if (reader.TokenType != JsonTokenType.StartObject)
        {
            throw new JsonException($"expected {{\"{tag}\": …}}, found {reader.TokenType}");
        }
        if (!reader.Read() || reader.TokenType != JsonTokenType.PropertyName || !reader.ValueTextEquals(tag))
        {
            throw new JsonException($"expected {{\"{tag}\": …}}");
        }
        if (!reader.Read() || reader.TokenType != JsonTokenType.String)
        {
            throw new JsonException($"`{tag}` holds text");
        }
        var text = reader.GetString()!;
        if (!reader.Read() || reader.TokenType != JsonTokenType.EndObject)
        {
            throw new JsonException($"{{\"{tag}\": …}} has one key");
        }
        return text;
    }

    internal static void WriteTag(Utf8JsonWriter writer, string tag, string text)
    {
        writer.WriteStartObject();
        writer.WriteString(tag, text);
        writer.WriteEndObject();
    }

    /// <summary>Text of a time with its fraction cut to the 100 ns .NET keeps: trunkdb keeps nanoseconds.</summary>
    internal static string Ticks(string text)
    {
        var dot = text.IndexOf('.');
        if (dot < 0)
        {
            return text;
        }
        var end = dot + 1;
        while (end < text.Length && char.IsAsciiDigit(text[end]))
        {
            end++;
        }
        return end - dot - 1 > 7 ? string.Concat(text.AsSpan(0, dot + 8), text.AsSpan(end)) : text;
    }

    internal const string Instant = "yyyy-MM-dd'T'HH:mm:ss.FFFFFFF'Z'";

    internal static DateTime ReadInstant(ref Utf8JsonReader reader)
    {
        var text = ReadTag(ref reader, "$date");
        if (!text.Contains('T'))
        {
            throw new JsonException($"{{\"$date\": \"{text}\"}} is a date without a time: read it as a DateOnly");
        }
        if (!DateTimeOffset.TryParse(Ticks(text), CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var parsed))
        {
            throw new JsonException($"{{\"$date\": \"{text}\"}} is not a date and time");
        }
        return parsed.UtcDateTime;
    }
}

internal sealed class DocIdConverter : JsonConverter<DocId>
{
    public override DocId Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var text = TrunkJson.ReadTag(ref reader, "$id");
        return DocId.TryParse(text, out var id) ? id : throw new JsonException($"{{\"$id\": \"{text}\"}} is not a UUID");
    }

    public override void Write(Utf8JsonWriter writer, DocId value, JsonSerializerOptions options) =>
        TrunkJson.WriteTag(writer, "$id", value.ToString());
}

/// <summary>An id as its text alone, as a report gives it.</summary>
internal sealed class DocIdTextConverter : JsonConverter<DocId>
{
    public override DocId Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        DocId.Parse(reader.GetString() ?? "");

    public override void Write(Utf8JsonWriter writer, DocId value, JsonSerializerOptions options) =>
        writer.WriteStringValue(value.ToString());
}

/// <summary>A <see cref="DateTime"/> as an instant in UTC: local time converted, unspecified taken as UTC.</summary>
internal sealed class DateTimeConverter : JsonConverter<DateTime>
{
    public override DateTime Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        TrunkJson.ReadInstant(ref reader);

    public override void Write(Utf8JsonWriter writer, DateTime value, JsonSerializerOptions options)
    {
        var utc = value.Kind switch
        {
            DateTimeKind.Local => value.ToUniversalTime(),
            DateTimeKind.Unspecified => DateTime.SpecifyKind(value, DateTimeKind.Utc),
            _ => value,
        };
        TrunkJson.WriteTag(writer, "$date", utc.ToString(TrunkJson.Instant, CultureInfo.InvariantCulture));
    }
}

/// <summary>A <see cref="DateTimeOffset"/> as an instant: read back in UTC, its offset not kept.</summary>
internal sealed class DateTimeOffsetConverter : JsonConverter<DateTimeOffset>
{
    public override DateTimeOffset Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        new(TrunkJson.ReadInstant(ref reader));

    public override void Write(Utf8JsonWriter writer, DateTimeOffset value, JsonSerializerOptions options) =>
        TrunkJson.WriteTag(writer, "$date", value.UtcDateTime.ToString(TrunkJson.Instant, CultureInfo.InvariantCulture));
}

internal sealed class DateOnlyConverter : JsonConverter<DateOnly>
{
    public override DateOnly Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var text = TrunkJson.ReadTag(ref reader, "$date");
        return DateOnly.TryParseExact(text, "yyyy-MM-dd", CultureInfo.InvariantCulture, DateTimeStyles.None, out var day)
            ? day
            : throw new JsonException($"{{\"$date\": \"{text}\"}} is not a date alone");
    }

    public override void Write(Utf8JsonWriter writer, DateOnly value, JsonSerializerOptions options) =>
        TrunkJson.WriteTag(writer, "$date", value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture));
}

internal sealed class TimeOnlyConverter : JsonConverter<TimeOnly>
{
    public override TimeOnly Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var text = TrunkJson.ReadTag(ref reader, "$time");
        return TimeOnly.TryParse(TrunkJson.Ticks(text), CultureInfo.InvariantCulture, DateTimeStyles.None, out var time)
            ? time
            : throw new JsonException($"{{\"$time\": \"{text}\"}} is not a time of day");
    }

    public override void Write(Utf8JsonWriter writer, TimeOnly value, JsonSerializerOptions options) =>
        TrunkJson.WriteTag(writer, "$time", value.ToString("HH:mm:ss.FFFFFFF", CultureInfo.InvariantCulture));
}

internal sealed class BinaryConverter : JsonConverter<byte[]>
{
    public override byte[] Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var text = TrunkJson.ReadTag(ref reader, "$binary");
        try
        {
            return Convert.FromBase64String(text);
        }
        catch (FormatException)
        {
            throw new JsonException($"{{\"$binary\": \"{text}\"}} is not base64");
        }
    }

    public override void Write(Utf8JsonWriter writer, byte[] value, JsonSerializerOptions options) =>
        TrunkJson.WriteTag(writer, "$binary", Convert.ToBase64String(value));
}

/// <summary>A float written with its fraction, so trunkdb keeps it a float; NaN and the infinities tagged.</summary>
internal sealed class DoubleConverter : JsonConverter<double>
{
    public override double Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        reader.TokenType == JsonTokenType.Number ? reader.GetDouble() : Special(TrunkJson.ReadTag(ref reader, "$float"));

    public override void Write(Utf8JsonWriter writer, double value, JsonSerializerOptions options)
    {
        if (double.IsFinite(value))
        {
            writer.WriteRawValue(Number(value.ToString("R", CultureInfo.InvariantCulture)));
        }
        else
        {
            TrunkJson.WriteTag(writer, "$float", double.IsNaN(value) ? "NaN" : value > 0 ? "Infinity" : "-Infinity");
        }
    }

    internal static double Special(string text) => text switch
    {
        "NaN" => double.NaN,
        "Infinity" => double.PositiveInfinity,
        "-Infinity" => double.NegativeInfinity,
        _ => throw new JsonException($"{{\"$float\": \"{text}\"}}: NaN, Infinity or -Infinity"),
    };

    /// <summary>A number's shortest text, <c>.0</c> added where it has neither fraction nor exponent.</summary>
    internal static string Number(string text) =>
        text.AsSpan().IndexOfAny('.', 'E') < 0 ? text + ".0" : text;
}

internal sealed class SingleConverter : JsonConverter<float>
{
    public override float Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        reader.TokenType == JsonTokenType.Number
            ? reader.GetSingle()
            : (float)DoubleConverter.Special(TrunkJson.ReadTag(ref reader, "$float"));

    public override void Write(Utf8JsonWriter writer, float value, JsonSerializerOptions options)
    {
        if (float.IsFinite(value))
        {
            writer.WriteRawValue(DoubleConverter.Number(value.ToString("R", CultureInfo.InvariantCulture)));
        }
        else
        {
            TrunkJson.WriteTag(writer, "$float", float.IsNaN(value) ? "NaN" : value > 0 ? "Infinity" : "-Infinity");
        }
    }
}
