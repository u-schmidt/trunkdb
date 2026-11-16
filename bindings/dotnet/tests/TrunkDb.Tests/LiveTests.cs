using System.Text.Json.Nodes;
using Xunit;

namespace TrunkDb.Tests;

public sealed class Van
{
    public DocId Id { get; set; }
    public string Name { get; set; } = "";
    public string Zone { get; set; } = "road";
    public double Speed { get; set; }
}

public sealed record ZoneCount(string Zone, ulong N);

public sealed class Device
{
    public DocId Id { get; set; }
    public string Name { get; set; } = "";
    public DateTime LastSeen { get; set; }
}

public sealed record Count(ulong N);

/// <summary>Changes as they happen (SPEC §194): subscriptions, live queries and live aggregates.</summary>
public sealed class LiveTests : IDisposable
{
    private static readonly TimeSpan Wait = TimeSpan.FromSeconds(10);

    private readonly string directory = Directory.CreateTempSubdirectory("trunkdb-dotnet-live-").FullName;

    public void Dispose() => Directory.Delete(directory, recursive: true);

    private Database Open() => Database.Open(Path.Combine(directory, "app.trunkdb"));

    /// <summary>Every event waiting now: each is there once its commit returns.</summary>
    private static List<TEvent> Waiting<TEvent>(LiveFeed<TEvent> feed) where TEvent : class
    {
        var all = new List<TEvent>();
        while (feed.Next(TimeSpan.Zero) is { } next)
        {
            all.Add(next);
        }
        Assert.False(feed.Ended);
        return all;
    }

    [Fact]
    public void A_live_query_reports_what_enters_leaves_and_changes()
    {
        using var db = Open();
        var vans = db.Collection<Van>("vans");
        var a = new Van { Name = "a", Zone = "depot" };
        var b = new Van { Name = "b" };
        vans.InsertMany([a, b]);

        using var inDepot = vans.Watch("zone = :z", new { z = "depot" });
        var first = Assert.IsType<LiveEvent<Van>.Entered>(Assert.Single(Waiting(inDepot)));
        Assert.Equal(a.Id, first.Id);
        Assert.Equal("a", first.Document.Name);
        Assert.Equal(a.Id, first.Document.Id);
        Assert.Null(first.At);

        b.Zone = "depot";
        vans.Update(b);
        a.Speed = 5;
        vans.Update(a);
        a.Zone = "road";
        vans.Update(a);
        vans.Delete(b);
        var events = Waiting(inDepot);
        Assert.Equal(4, events.Count);
        var entered = Assert.IsType<LiveEvent<Van>.Entered>(events[0]);
        Assert.Equal((b.Id, "b"), (entered.Id, entered.Document.Name));
        Assert.Equal(5, Assert.IsType<LiveEvent<Van>.Changed>(events[1]).Document.Speed);
        var left = Assert.IsType<LiveEvent<Van>.Left>(events[2]);
        Assert.Equal((a.Id, "road"), (left.Id, left.Document!.Zone));
        var gone = Assert.IsType<LiveEvent<Van>.Left>(events[3]);
        Assert.Equal(b.Id, gone.Id);
        Assert.Null(gone.Document);
        Assert.Null(inDepot.Next(TimeSpan.FromMilliseconds(20)));
        Assert.False(inDepot.Ended, "nothing yet is not an end");
        Assert.Throws<ArgumentOutOfRangeException>(() => inDepot.Next(TimeSpan.FromSeconds(-2)));
    }

    [Fact]
    public void A_sorted_live_query_is_a_list_that_says_where()
    {
        using var db = Open();
        var vans = db.Collection<Van>("vans");
        var slow = new Van { Name = "slow", Speed = 1 };
        vans.Insert(slow);
        using var fastest = vans.Watch("order by speed desc limit 1");
        Assert.Equal(0, Assert.IsType<LiveEvent<Van>.Entered>(Assert.Single(Waiting(fastest))).At);

        var fast = new Van { Name = "fast", Speed = 9 };
        vans.Insert(fast);
        var events = Waiting(fastest);
        Assert.Equal(2, events.Count);
        Assert.Equal((slow.Id, 0), (events[0].Id, Assert.IsType<LiveEvent<Van>.Left>(events[0]).From));
        Assert.Equal((fast.Id, 0), (events[1].Id, Assert.IsType<LiveEvent<Van>.Entered>(events[1]).At));

        var refused = Assert.Throws<TrunkDbException>(() => vans.Watch("order by speed limit 5 offset 1"));
        Assert.Equal(ErrorCode.InvalidArgument, refused.Code);
        Assert.Throws<ParseException>(() => vans.Watch("zone ="));
    }

    [Fact]
    public void A_live_aggregate_reports_its_rows()
    {
        using var db = Open();
        var vans = db.Collection<Van>("vans");
        vans.Insert(new Van { Name = "a", Zone = "depot" });
        using var perZone = vans.WatchAggregate<ZoneCount>(null, "count() as n group by zone having n >= :least", new { least = 1 });
        var first = Assert.IsType<RowEvent<ZoneCount>.Entered>(Assert.Single(Waiting(perZone)));
        Assert.Equal(new ZoneCount("depot", 1), first.Row);
        Assert.Equal("depot", first.Group["zone"]!.GetValue<string>());

        var b = new Van { Name = "b", Zone = "depot" };
        vans.Insert(b);
        Assert.Equal(new ZoneCount("depot", 2), Assert.IsType<RowEvent<ZoneCount>.Changed>(Assert.Single(Waiting(perZone))).Row);
        vans.DeleteMany(null);
        var left = Assert.IsType<RowEvent<ZoneCount>.Left>(Assert.Single(Waiting(perZone)));
        Assert.Null(left.Row);

        using var whole = vans.WatchAggregate<JsonObject>(null, "count() as n");
        var one = Assert.IsType<RowEvent<JsonObject>.Entered>(Assert.Single(Waiting(whole)));
        Assert.Empty(one.Group);
        Assert.Equal(0, one.Row["n"]!.GetValue<int>());
    }

    [Fact]
    public void A_subscription_hears_of_every_commit()
    {
        using var db = Open();
        using var changes = db.Subscribe();
        var vans = db.Collection<Van>("vans");
        var a = new Van { Name = "a" };
        vans.Insert(a);
        a.Speed = 3;
        vans.Update(a);
        vans.Drop();
        Assert.Equal(
            [
                new Change(ChangeKind.Inserted, "vans", a.Id),
                new Change(ChangeKind.Updated, "vans", a.Id),
                new Change(ChangeKind.Dropped, "vans", null),
            ],
            Waiting(changes).SelectMany(commit => commit.Changes));
    }

    [Fact]
    public void A_feed_ends_with_its_database_once_its_events_are_read()
    {
        var db = Open();
        var changes = db.Subscribe();
        db.Collection<Van>("vans").Insert(new Van());
        db.Dispose();
        Assert.NotNull(changes.Next(TimeSpan.Zero));
        Assert.False(changes.Ended);
        Assert.Null(changes.Next(Timeout.InfiniteTimeSpan));
        Assert.True(changes.Ended);
        Assert.Empty(changes.ReadAll());
        changes.Dispose();
        Assert.Throws<ObjectDisposedException>(() => changes.Next(TimeSpan.Zero));
    }

    [Fact]
    public async Task Await_foreach_reads_as_commits_come_and_stops_when_cancelled()
    {
        using var db = Open();
        var vans = db.Collection<Van>("vans");
        using var inDepot = vans.Watch("zone = \"depot\"");
        using var cancel = new CancellationTokenSource(Wait);
        var seen = new List<string>();
        var reading = Task.Run(async () =>
        {
            await foreach (var e in inDepot.ReadAllAsync(cancel.Token))
            {
                seen.Add(((LiveEvent<Van>.Entered)e).Document.Name);
                if (seen.Count == 2)
                {
                    break;
                }
            }
        });
        vans.Insert(new Van { Name = "a", Zone = "depot" });
        vans.Insert(new Van { Name = "b", Zone = "road" });
        vans.Insert(new Van { Name = "c", Zone = "depot" });
        await reading.WaitAsync(Wait);
        Assert.Equal(["a", "c"], seen);

        // Left early: what its reader took ahead is kept, in order, for
        // the next read.
        vans.InsertMany(new[] { "d", "e", "f" }.Select(name => new Van { Name = name, Zone = "depot" }));
        await foreach (var e in inDepot.ReadAllAsync(cancel.Token))
        {
            Assert.Equal("d", ((LiveEvent<Van>.Entered)e).Document.Name);
            break;
        }
        Assert.Equal(["e", "f"], Waiting(inDepot).Select(e => ((LiveEvent<Van>.Entered)e).Document.Name));

        // Cancelled while waiting.
        using var soon = new CancellationTokenSource(TimeSpan.FromMilliseconds(50));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(async () =>
        {
            await foreach (var _ in inDepot.ReadAllAsync(soon.Token))
            {
            }
        });
    }

    [Fact]
    public async Task A_feed_left_by_a_loop_has_not_ended_while_it_keeps_events()
    {
        var db = Open();
        using var changes = db.Subscribe();
        var vans = db.Collection<Van>("vans");
        foreach (var name in new[] { "a", "b", "c" })
        {
            vans.Insert(new Van { Name = name });
        }
        db.Dispose();
        await foreach (var _ in changes.ReadAllAsync())
        {
            // Time for the loop's reader to take the rest and meet the end.
            Thread.Sleep(TimeSpan.FromMilliseconds(500));
            break;
        }
        Assert.False(changes.Ended, "two events are kept");
        var rest = new List<Commit>();
        while (!changes.Ended)
        {
            if (changes.Next(Timeout.InfiniteTimeSpan) is { } commit)
            {
                rest.Add(commit);
            }
        }
        Assert.Equal(2, rest.Count);
    }

    [Fact]
    public void A_foreach_on_another_thread_ends_when_the_feed_is_disposed()
    {
        using var db = Open();
        var changes = db.Subscribe();
        var count = 0;
        var reader = new Thread(() =>
        {
            foreach (var _ in changes.ReadAll())
            {
                count++;
            }
        });
        reader.Start();
        db.Collection<Van>("vans").Insert(new Van());
        SpinWait.SpinUntil(() => Volatile.Read(ref count) == 1, Wait);
        changes.Dispose();
        Assert.True(reader.Join(Wait), "the loop ended");
        Assert.Equal(1, count);
    }

    /// <summary>
    /// A live query and aggregate against <c>now</c> (SPEC §204, §205, §206): read again
    /// as time passes, with nothing written, in the writer's process and beside it.
    /// </summary>
    [Fact]
    public void A_live_query_against_now_sees_a_device_go_silent()
    {
        var age = TimeSpan.FromMilliseconds(300);
        using var db = Open();
        var devices = db.Collection<Device>("devices");
        devices.EnsureIndex("lastSeen");
        var van = new Device { Name = "van", LastSeen = DateTime.UtcNow };
        devices.Insert(van);

        using (var silent = devices.Watch("lastSeen < now - 300ms"))
        using (var reporting = devices.WatchAggregate<Count>("lastSeen >= now - 300ms", "count() as n"))
        {
            Assert.Empty(Waiting(silent));
            Assert.Equal(new Count(1), Assert.IsType<RowEvent<Count>.Entered>(Assert.Single(Waiting(reporting))).Row);
            var entered = Assert.IsType<LiveEvent<Device>.Entered>(silent.Next(Wait));
            Assert.Equal(van.Id, entered.Id);
            Assert.Equal(van.LastSeen, entered.Document.LastSeen);
            Assert.True(DateTime.UtcNow - van.LastSeen > age, "never before its moment");
            Assert.Equal(new Count(0), Assert.IsType<RowEvent<Count>.Changed>(reporting.Next(Wait)).Row);
        }

        // Beside the writer, read-only: the look reads it again at the moment.
        using var reader = Database.Open(Path.Combine(directory, "app.trunkdb"), new OpenOptions { ReadOnly = true });
        using var beside = reader.Collection<Device>("devices").Watch("lastSeen < now - 300ms");
        Assert.Equal(van.Id, Assert.IsType<LiveEvent<Device>.Entered>(beside.Next(Wait)).Id);
        van.LastSeen = DateTime.UtcNow;
        devices.Update(van);
        Assert.Equal(van.Id, Assert.IsType<LiveEvent<Device>.Left>(beside.Next(Wait)).Id);
        var again = Assert.IsType<LiveEvent<Device>.Entered>(beside.Next(Wait));
        Assert.Equal(van.LastSeen, again.Document.LastSeen);
        Assert.True(DateTime.UtcNow - van.LastSeen > age, "never before its moment");
    }
}
