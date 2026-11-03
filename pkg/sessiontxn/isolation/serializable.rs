/// MVB: `SERIALIZABLE` is accepted and behaves like optimistic RR
/// (statement snapshots stay RR; TiDB oracle-like SI / pessimistic
/// SERIALIZABLE remains out of scope). Marker matches TiDB
/// `PessimisticSerializableTxnContextProvider` naming only.
pub struct SerializableTxnContextProvider;
