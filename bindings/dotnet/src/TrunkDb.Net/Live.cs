using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading.Channels;

namespace TrunkDb;

/// <summary>
/// Changes as they happen (SPEC §194): events trunkdb reports as each
/// commit commits, read here one at a time. Nothing is missed between two
/// reads, however late they come: events wait in memory until read, and a
/// writer never waits for them. Dispose it to stop them; disposing the
/// database ends it too, once the events before are read.
/// <para>
/// Read it with <see cref="Next"/>, with <c>foreach</c> over
/// <see cref="ReadAll"/> on a thread of its own, or with <c>await foreach</c>
/// over <see cref="ReadAllAsync"/>, as a <see cref="ChannelReader{T}"/> is
/// read. Each loop ends when the feed does; an error that ends it is thrown
/// once.
/// </para>
/// </summary>
public abstract class LiveFeed<TEvent> : IDisposable
    where TEvent : class
{
    /// <summary>How long one wait of an enumeration lasts before it looks whether it should stop.</summary>
    private static readonly TimeSpan Slice = TimeSpan.FromMilliseconds(100);

    private readonly LiveHandle handle;
    private readonly Queue<TEvent> kept = new();
    private volatile bool ended;

    private protected LiveFeed(LiveHandle handle) => this.handle = handle;

    /// <summary>
    /// True once it has ended, every event read: the database was disposed,
    /// or an error ended it. A <see cref="Next"/> that returned null before
    /// its time was up says so here. Events a loop read ahead and kept
    /// (<see cref="ReadAllAsync"/>) count as not yet read.
    /// </summary>
    public bool Ended
    {
        get
        {
            lock (kept)
            {
                return ended && kept.Count == 0;
            }
        }
    }

    /// <summary>
    /// The next event, waiting at most <paramref name="timeout"/>:
    /// <see cref="TimeSpan.Zero"/> not at all, <see cref="Timeout.InfiniteTimeSpan"/>
    /// until one comes or it ends. Null if none came in time, or it has
    /// ended: <see cref="Ended"/> says which.
    /// </summary>
    /// <exception cref="TrunkDbException">The error that ended it, once.</exception>
    /// <exception cref="ObjectDisposedException">It was disposed.</exception>
    public TEvent? Next(TimeSpan timeout)
    {
        long ms = timeout == Timeout.InfiniteTimeSpan ? -1
            : timeout >= TimeSpan.Zero ? (long)Math.Ceiling(timeout.TotalMilliseconds)
            : throw new ArgumentOutOfRangeException(nameof(timeout), timeout, "a wait is zero or more, or infinite");
        ObjectDisposedException.ThrowIf(handle.IsClosed, this);
        lock (kept)
        {
            if (kept.Count > 0)
            {
                return kept.Dequeue();
            }
        }
        if (ended)
        {
            return null;
        }
        Native.Check(Native.trunkdb_live_next(handle, ms, out var text, out var over));
        if (Native.Take(text) is { } json)
        {
            return Parse(json);
        }
        ended = over;
        return null;
    }

    /// <summary>
    /// Each event in turn, waiting for the next; the loop ends when the
    /// feed does, or when it is disposed from another thread.
    /// </summary>
    public IEnumerable<TEvent> ReadAll()
    {
        while (Poll(out var next))
        {
            if (next is not null)
            {
                yield return next;
            }
        }
    }

    /// <summary>
    /// Each event in turn, read on a thread of its own while the loop
    /// runs; it ends when the feed does, and throws
    /// <see cref="OperationCanceledException"/> when
    /// <paramref name="cancellationToken"/> is cancelled. Events read but
    /// not yet handed out when the loop is left are kept for the next read.
    /// </summary>
    public async IAsyncEnumerable<TEvent> ReadAllAsync(
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var events = Channel.CreateUnbounded<TEvent>(new UnboundedChannelOptions { SingleReader = true, SingleWriter = true });
        var stop = false;
        var done = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var reader = new Thread(() =>
        {
            try
            {
                while (!Volatile.Read(ref stop) && !cancellationToken.IsCancellationRequested && Poll(out var next))
                {
                    if (next is not null)
                    {
                        events.Writer.TryWrite(next);
                    }
                }
                events.Writer.TryComplete();
            }
            catch (Exception e)
            {
                events.Writer.TryComplete(e);
            }
            finally
            {
                done.SetResult();
            }
        })
        {
            IsBackground = true,
            Name = "trunkdb-live",
        };
        reader.Start();
        try
        {
            await foreach (var next in events.Reader.ReadAllAsync(cancellationToken).ConfigureAwait(false))
            {
                yield return next;
            }
        }
        finally
        {
            Volatile.Write(ref stop, true);
            await done.Task.ConfigureAwait(false);
            // In a method of its own: in an async iterator's finally, left
            // by a break, a try/finally within (a lock, a foreach) ends the
            // whole block where it ends, and what follows would be skipped.
            Keep(events.Reader);
        }
    }

    /// <summary>What a reader took and didn't hand out, put first for the next read.</summary>
    private void Keep(ChannelReader<TEvent> left)
    {
        lock (kept)
        {
            var all = new List<TEvent>();
            while (left.TryRead(out var next))
            {
                all.Add(next);
            }
            all.AddRange(kept);
            kept.Clear();
            all.ForEach(kept.Enqueue);
        }
    }

    /// <summary>One wait of an enumeration: false once it has ended or was disposed.</summary>
    private bool Poll(out TEvent? next)
    {
        next = null;
        if (handle.IsClosed)
        {
            return false;
        }
        try
        {
            next = Next(Slice);
        }
        catch (ObjectDisposedException)
        {
            return false;
        }
        return next is not null || !ended;
    }

    /// <summary>Stops the events: none is received after.</summary>
    public void Dispose() => handle.Dispose();

    private protected abstract TEvent Parse(string json);
}

/// <summary>
/// A filter kept live (SPEC §138, §194), from <see cref="Collection{T}.Watch"/>:
/// first an <see cref="LiveEvent{T}.Entered"/> for every document it matches
/// now, then, as each commit commits, the documents that came to match it,
/// that left it, and that were written and still match.
/// <code>
/// using var inDepot = vans.Watch("zone = :z", new { z = "depot" });
/// await foreach (var e in inDepot.ReadAllAsync(token))
/// {
///     switch (e)
///     {
///         case LiveEvent&lt;Van&gt;.Entered entered: Console.WriteLine($"{entered.Document.Name} arrived"); break;
///         case LiveEvent&lt;Van&gt;.Left left: Console.WriteLine($"{left.Id} left"); break;
///     }
/// }
/// </code>
/// </summary>
public sealed class LiveQuery<T> : LiveFeed<LiveEvent<T>>
{
    private readonly JsonSerializerOptions json;

    internal LiveQuery(LiveHandle handle, JsonSerializerOptions json) : base(handle) => this.json = json;

    private protected override LiveEvent<T> Parse(string text)
    {
        var e = JsonNode.Parse(text)!.AsObject();
        var id = DocId.Parse(e["id"]!.GetValue<string>());
        T? Document() => e["doc"] is { } doc ? doc.Deserialize<T>(json) : default;
        int? Place(string name) => e[name]?.GetValue<int>();
        return e["event"]!.GetValue<string>() switch
        {
            "entered" => new LiveEvent<T>.Entered(id, Document()!, Place("at")),
            "left" => new LiveEvent<T>.Left(id, Document(), Place("from")),
            "changed" => new LiveEvent<T>.Changed(id, Document()!, Place("from"), Place("at")),
            var other => throw new InvalidOperationException($"trunkdb reported an event this wrapper doesn't know: {other}"),
        };
    }
}

/// <summary>
/// What a live query reports (SPEC §138): a document that came to match its
/// filter, one that left it, or one written that still matches; each as the
/// commit left it. With a sort the query is a list, and each event says
/// where: take a document out at <c>From</c>, put it in at <c>At</c>.
/// </summary>
public abstract record LiveEvent<T>
{
    private LiveEvent(DocId id) => Id = id;

    /// <summary>The document's id.</summary>
    public DocId Id { get; }

    /// <summary>It matches, and didn't before; or it matched when the watch began.</summary>
    public sealed record Entered(DocId Id, T Document, int? At) : LiveEvent<T>(Id);

    /// <summary>It matched before, and doesn't now; its document null if it was deleted.</summary>
    public sealed record Left(DocId Id, T? Document, int? From) : LiveEvent<T>(Id);

    /// <summary>It matched before and still does, and the commit wrote it.</summary>
    public sealed record Changed(DocId Id, T Document, int? From, int? At) : LiveEvent<T>(Id);
}

/// <summary>
/// An aggregate kept live (SPEC §139, §194), from
/// <see cref="Collection{T}.WatchAggregate{TRow}"/>: first an
/// <see cref="RowEvent{TRow}.Entered"/> for every row, then, as each commit
/// to the collection commits, the rows that came, left or changed.
/// </summary>
public sealed class LiveAggregate<TRow> : LiveFeed<RowEvent<TRow>>
{
    private readonly JsonSerializerOptions json;

    internal LiveAggregate(LiveHandle handle, JsonSerializerOptions json) : base(handle) => this.json = json;

    private protected override RowEvent<TRow> Parse(string text)
    {
        var e = JsonNode.Parse(text)!.AsObject();
        var group = e["group"]!.AsObject();
        e.Remove("group");
        TRow? Row() => e["row"] is { } row ? row.Deserialize<TRow>(json) : default;
        return e["event"]!.GetValue<string>() switch
        {
            "entered" => new RowEvent<TRow>.Entered(group, Row()!),
            "left" => new RowEvent<TRow>.Left(group, Row()),
            "changed" => new RowEvent<TRow>.Changed(group, Row()!),
            var other => throw new InvalidOperationException($"trunkdb reported an event this wrapper doesn't know: {other}"),
        };
    }
}

/// <summary>
/// What a live aggregate reports (SPEC §139): a row known by its
/// <c>group by</c> values, <see cref="Group"/>, empty without one.
/// </summary>
public abstract record RowEvent<TRow>
{
    private RowEvent(JsonObject group) => Group = group;

    /// <summary>The row's group values, as trunkdb's JSON.</summary>
    public JsonObject Group { get; }

    /// <summary>The row is in the result, and wasn't before; or it was when the watch began.</summary>
    public sealed record Entered(JsonObject Group, TRow Row) : RowEvent<TRow>(Group);

    /// <summary>The row was in the result, and isn't now; null where its group has no documents left.</summary>
    public sealed record Left(JsonObject Group, TRow? Row) : RowEvent<TRow>(Group);

    /// <summary>The row was in the result and still is, with other values.</summary>
    public sealed record Changed(JsonObject Group, TRow Row) : RowEvent<TRow>(Group);
}

/// <summary>
/// Every commit from when it was made (SPEC §135, §194), from
/// <see cref="Database.Subscribe"/>: the documents each one inserted,
/// updated or deleted, and the collections it dropped.
/// </summary>
public sealed class Changes : LiveFeed<Commit>
{
    internal Changes(LiveHandle handle) : base(handle) { }

    private protected override Commit Parse(string text)
    {
        var e = JsonNode.Parse(text)!.AsObject();
        var changes = e["changes"]!.AsArray().Select(node =>
        {
            var change = node!.AsObject();
            var kind = change["change"]!.GetValue<string>() switch
            {
                "inserted" => ChangeKind.Inserted,
                "updated" => ChangeKind.Updated,
                "deleted" => ChangeKind.Deleted,
                "dropped" => ChangeKind.Dropped,
                var other => throw new InvalidOperationException($"trunkdb reported a change this wrapper doesn't know: {other}"),
            };
            DocId? id = change["id"] is { } given ? DocId.Parse(given.GetValue<string>()) : null;
            return new Change(kind, change["collection"]!.GetValue<string>(), id);
        });
        return new Commit(changes.ToList());
    }
}

/// <summary>One committed batch: each document it changed once, with its net effect.</summary>
public sealed record Commit(IReadOnlyList<Change> Changes);

/// <summary>A document a commit changed, or a collection it dropped (<see cref="Id"/> null).</summary>
public sealed record Change(ChangeKind Kind, string Collection, DocId? Id);

/// <summary>What a commit did to a document, or to a collection.</summary>
public enum ChangeKind
{
    /// <summary>It wasn't there before the batch, and is after it.</summary>
    Inserted,

    /// <summary>It was there before and after, written in between.</summary>
    Updated,

    /// <summary>It was there before the batch, and isn't after it.</summary>
    Deleted,

    /// <summary>The collection and every document in it were dropped.</summary>
    Dropped,
}
