using System.Text.Json;
using System.Text.Json.Serialization;
using Xunit;

namespace TrunkDb.Tests;

/// <summary>The tags, written and read without the library (SPEC §180).</summary>
public class JsonTests
{
    private static readonly JsonSerializerOptions Options = TrunkJson.Options();

    private static string Write<T>(T value) => JsonSerializer.Serialize(value, Options);

    private static T Read<T>(string json) => JsonSerializer.Deserialize<T>(json, Options)!;

    [Fact]
    public void Instants_are_utc_with_only_the_fraction_they_have()
    {
        var at = new DateTime(2026, 11, 9, 14, 5, 0, DateTimeKind.Utc);
        Assert.Equal("""{"$date":"2026-11-09T14:05:00Z"}""", Write(at));
        Assert.Equal("""{"$date":"2026-11-09T14:05:00.25Z"}""", Write(at.AddMilliseconds(250)));
        Assert.Equal("""{"$date":"2026-11-09T14:05:00.0000001Z"}""", Write(at.AddTicks(1)));
        // Unspecified is taken as UTC; local is converted.
        Assert.Equal(Write(at), Write(DateTime.SpecifyKind(at, DateTimeKind.Unspecified)));
        Assert.Equal(Write(at), Write(at.ToLocalTime()));
        Assert.Equal(Write(at), Write(new DateTimeOffset(at).ToOffset(TimeSpan.FromHours(2))));

        var read = Read<DateTime>("""{"$date":"2026-11-09T14:05:00.123456789Z"}""");
        Assert.Equal(DateTimeKind.Utc, read.Kind);
        Assert.Equal(at.AddTicks(1234567), read);
        Assert.Equal(TimeSpan.Zero, Read<DateTimeOffset>("""{"$date":"2026-11-09T14:05:00Z"}""").Offset);
        var day = Assert.Throws<JsonException>(() => Read<DateTime>("""{"$date":"2026-11-09"}"""));
        Assert.Contains("DateOnly", day.Message);
    }

    [Fact]
    public void Dates_and_times_of_day_have_tags_of_their_own()
    {
        Assert.Equal("""{"$date":"2026-11-09"}""", Write(new DateOnly(2026, 11, 9)));
        Assert.Equal("""{"$time":"14:05:00"}""", Write(new TimeOnly(14, 5)));
        Assert.Equal("""{"$time":"14:05:00.25"}""", Write(new TimeOnly(14, 5, 0, 250)));
        Assert.Equal(new DateOnly(2026, 11, 9), Read<DateOnly>("""{"$date":"2026-11-09"}"""));
        Assert.Equal(new TimeOnly(14, 5, 0, 250), Read<TimeOnly>("""{"$time":"14:05:00.250000000"}"""));
        Assert.Throws<JsonException>(() => Read<DateOnly>("""{"$date":"2026-11-09T14:05:00Z"}"""));
    }

    [Fact]
    public void Floats_stay_floats_and_nan_is_tagged()
    {
        Assert.Equal("3.0", Write(3.0));
        Assert.Equal("-0.0", Write(-0.0));
        Assert.Equal("0.1", Write(0.1));
        Assert.Equal("10000000000000000.0", Write(1e16));
        Assert.Equal("1E+300", Write(1e300));
        Assert.Equal("2.5", Write(2.5f));
        Assert.Equal("""{"$float":"NaN"}""", Write(double.NaN));
        Assert.Equal("""{"$float":"-Infinity"}""", Write(float.NegativeInfinity));
        Assert.True(double.IsPositiveInfinity(Read<double>("""{"$float":"Infinity"}""")));
        Assert.Equal(3.0, Read<double>("3"));
        Assert.Throws<JsonException>(() => Read<double>("""{"$float":"lots"}"""));
    }

    [Fact]
    public void Bytes_are_base64_and_ids_are_tagged()
    {
        Assert.Equal("""{"$binary":"AQID"}""", Write(new byte[] { 1, 2, 3 }));
        Assert.Equal(new byte[] { 1, 2, 3 }, Read<byte[]>("""{"$binary":"AQID"}"""));
        var id = DocId.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6");
        Assert.Equal("""{"$id":"01a1bc72-c792-772e-a864-a359a7ffaeb6"}""", Write(id));
        Assert.Equal(id, Read<DocId>("""{"$id":"01a1bc72-c792-772e-a864-a359a7ffaeb6"}"""));
        Assert.Throws<JsonException>(() => Read<DocId>("\"01a1bc72-c792-772e-a864-a359a7ffaeb6\""));
        Assert.Throws<JsonException>(() => Read<DocId>("""{"$id":"x","more":1}"""));
    }

    private sealed class Owned
    {
        public DocId Id { get; set; }
        public DocId Owner { get; set; }
        [JsonPropertyName("ref")]
        public DocId? Ref { get; set; }
    }

    private sealed record Named(DocId Id, string Title);

    [Fact]
    public void A_property_id_is_the_documents_id_and_left_out_when_empty()
    {
        var owner = DocId.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6");
        Assert.Equal(
            """{"owner":{"$id":"01a1bc72-c792-772e-a864-a359a7ffaeb6"},"ref":null}""",
            Write(new Owned { Owner = owner }));
        Assert.StartsWith("""{"_id":{"$id":"01a1bc72""", Write(new Owned { Id = owner, Owner = owner }));
        var named = Read<Named>("""{"_id":{"$id":"01a1bc72-c792-772e-a864-a359a7ffaeb6"},"title":"x"}""");
        Assert.Equal(owner, named.Id);
        Assert.Equal("x", named.Title);
    }

    [Fact]
    public void Ids_sort_as_their_text()
    {
        string[] texts =
        [
            "01a1bc72-c792-772e-a864-a359a7ffaeb6",
            "01a1bc72-c792-772e-a864-a359a7ffaeb7",
            "01a1bc73-0000-772e-a864-a359a7ffaeb6",
            "ffa1bc72-c792-772e-a864-a359a7ffaeb6",
            "01a1bc72-c793-772e-a864-a359a7ffaeb6",
            "01a1bc72-c792-872e-a864-a359a7ffaeb6",
        ];
        var byText = texts.OrderBy(t => t, StringComparer.Ordinal).ToArray();
        var byId = texts.Select(DocId.Parse).Order().Select(id => id.ToString()).ToArray();
        Assert.Equal(byText, byId);
        Assert.True(default(DocId).IsEmpty);
        Assert.Throws<FormatException>(() => DocId.Parse("nope"));
    }
}
