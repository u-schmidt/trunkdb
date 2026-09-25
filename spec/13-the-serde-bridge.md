# 13. The serde bridge (`serde_bridge.rs`)

Real and tested: `to_document`/`from_document` convert any `T: Serialize +
DeserializeOwned` to/from `Document`, using the same technique
`serde_json` uses for `serde_json::Value` and `bson` uses for
`bson::Bson` — implement serde's `Serializer` trait so its *output* is
`Document` instead of bytes/text, and implement `Deserializer` *for*
`Document` itself so it can feed derive-generated code directly. This
closes out the one item §4.1 originally flagged as "real, nontrivial
work, deliberately not rushed."

## 13.1 Enum representation: externally tagged, matching `serde_json`'s default
A unit variant serializes as a bare string (`Shape::Point` →
`Document::String("Point")`); every other variant kind serializes as a
single-entry object (`Shape::Circle(2.0)` → `{"Circle": 2.0}`,
`Shape::Rect{w,h}` → `{"Rect": {"w":..,"h":..}}`). Not invented for this
project — it's the same "externally tagged" default `serde_json` and most
other value-type bridges use, chosen deliberately so the behavior is
already familiar rather than a new convention to learn.

## 13.2 Why `deserialize_any` does most of the work
`Document` is *self-describing* — unlike a byte-stream `Deserializer`,
where `deserialize_i64` means "read exactly 8 bytes here," `Document`
already knows what it is regardless of which method serde calls. So
`deserialize_any` is the one method that actually inspects `self` and
calls the matching `visitor.visit_*`; nearly every other required method
(`deserialize_bool`, `deserialize_struct`, `deserialize_seq`, ...) just
forwards to it via `serde::forward_to_deserialize_any!`. Only
`deserialize_option` (`Null` → none, else some) and `deserialize_enum`
(reconstructing the bare-string/single-entry-object shape from §13.1)
needed real logic.

## 13.3 Wiring `Collection<T>`: convert, then delegate
`Collection<T>`'s methods (`insert`/`get`/`update`/`delete`/`find`) are
thin: convert via `to_document`/`from_document`, then delegate to a
freshly-built `Collection<'db, Document>` (same `db`, a cloned `name` —
cheap, consistent with `Collection` being a handle constructed freely
rather than held onto) for the actual catalog/index/data-page work. The
storage logic exists in exactly one place (§12) regardless of whether the
caller used the typed or untyped path.

## 13.4 A real gotcha: `IntoDeserializer` isn't automatic
Expected serde to provide a blanket `impl<T: Deserializer> IntoDeserializer
for T`, needed so `serde::de::value::SeqDeserializer`/`MapDeserializer`
(serde's built-in "visit this iterator as a seq/map" helpers, used to
implement `Array`/`Object` deserialization without hand-writing
`SeqAccess`/`MapAccess`) could wrap iterators of `Document`. That blanket
impl doesn't exist — every value-type bridge, `serde_json::Value`
included, writes the one-line `impl IntoDeserializer for Document { fn
into_deserializer(self) -> Self { self } }` itself. First compile attempt
failed on exactly this.

## 13.5 `DocId` has no special wire representation
A stored `Document::Id` decodes as its plain string form (`id.to_string()`)
when flowing into an arbitrary `T` field — there's no dedicated "this was
an id" signal preserved through the bridge (that would need a sentinel
newtype-name technique, e.g. what `serde_bytes` does for `Vec<u8>` vs.
`serde_bytes::Bytes` — real, but disproportionate complexity for v0).
`DocId` itself also has no hand-written `Serialize`/`Deserialize` impl
yet, so a `T` struct can't yet have a field of type `DocId` directly.

Partially revisited (§18): the untyped path now merges `_id` into every
stored `Object` document, so `Collection<Document>` no longer has this
gap. The typed path still does — a `T` struct still can't declare its own
`id: DocId` field and have it populated, since that needs both the
`Serialize`/`Deserialize` impl mentioned above and a decision about which
field name/convention `insert` would populate. Worth revisiting only if a
real struct actually wants an embedded id field — less likely since
`find_with_ids` (§23) hands ids out alongside `T`.
