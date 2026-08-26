//! UUIDv7-backed typed identifiers.
//!
//! Wire form is the lowercase hyphenated UUID string. Parse accepts only that
//! canonical form; generation always uses UUIDv7.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use uuid::Uuid;

/// Opaque UUIDv7 identifier parameterized by a marker type.
pub struct Id<T>(Uuid, PhantomData<fn() -> T>);

/// Parse failure for a non-canonical identifier string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdParseError;

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed UUID identifier")
    }
}

impl std::error::Error for IdParseError {}

impl<T> Id<T> {
    /// Allocate a fresh UUIDv7 identifier.
    ///
    /// `Default` is intentionally omitted: a nil UUID is not a valid allocated
    /// identity, and generating a random ID from `default()` is surprising.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::from_uuid(Uuid::now_v7())
    }

    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value, PhantomData)
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }

    fn write_canonical(uuid: Uuid, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; 36];
        f.write_str(uuid.hyphenated().encode_lower(&mut buf))
    }

    fn canonical_string(uuid: Uuid) -> String {
        let mut buf = [0u8; 36];
        uuid.hyphenated().encode_lower(&mut buf).to_owned()
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> Eq for Id<T> {}

impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T> Hash for Id<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Id").field(&self.0).finish()
    }
}

impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Self::write_canonical(self.0, f)
    }
}

impl<T> FromStr for Id<T> {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_canonical_uuid(s).map(Self::from_uuid)
    }
}

impl<T> Serialize for Id<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&Self::canonical_string(self.0))
    }
}

impl<'de, T> Deserialize<'de> for Id<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(IdVisitor(PhantomData))
    }
}

struct IdVisitor<T>(PhantomData<fn() -> T>);

impl<T> Visitor<'_> for IdVisitor<T> {
    type Value = Id<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a lowercase hyphenated UUID string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        parse_canonical_uuid(value)
            .map(Id::from_uuid)
            .map_err(E::custom)
    }
}

fn parse_canonical_uuid(s: &str) -> Result<Uuid, IdParseError> {
    let uuid = Uuid::try_parse(s).map_err(|_| IdParseError)?;
    let mut buf = [0u8; 36];
    let canonical = uuid.hyphenated().encode_lower(&mut buf);
    if canonical == s {
        Ok(uuid)
    } else {
        Err(IdParseError)
    }
}

macro_rules! typed_id {
    ($name:ident, $tag:ident) => {
        #[doc = concat!("Marker for [`", stringify!($name), "`].")]
        pub enum $tag {}

        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(Id<$tag>);

        impl $name {
            /// Allocate a fresh UUIDv7 identifier.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(Id::new())
            }

            pub const fn from_uuid(value: Uuid) -> Self {
                Self(Id::from_uuid(value))
            }

            pub const fn as_uuid(&self) -> Uuid {
                self.0.as_uuid()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Id::from_str(s).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Id::deserialize(deserializer).map(Self)
            }
        }
    };
}

// SDD §5 canonical UUID identifiers plus domain-model runtime IDs.
// `ArtifactId` is content-addressed (sha256) and is not defined here.
typed_id!(SessionId, SessionTag);
typed_id!(AgentId, AgentTag);
typed_id!(GoalId, GoalTag);
typed_id!(EvidenceId, EvidenceTag);
typed_id!(EventId, EventTag);
typed_id!(WorkspaceViewId, WorkspaceViewTag);
typed_id!(JobId, JobTag);
typed_id!(LeaseId, LeaseTag);
typed_id!(KnowledgeId, KnowledgeTag);
typed_id!(HandoffId, HandoffTag);
typed_id!(ControlLeaseId, ControlLeaseTag);
typed_id!(TrajectoryId, TrajectoryTag);
typed_id!(ProjectId, ProjectTag);
typed_id!(TurnId, TurnTag);
typed_id!(RepoId, RepoTag);
typed_id!(ContextItemId, ContextItemTag);
typed_id!(MessageId, MessageTag);
typed_id!(RuntimeId, RuntimeTag);
typed_id!(TraceId, TraceTag);
typed_id!(GraphId, GraphTag);
typed_id!(NodeId, NodeTag);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde::de::DeserializeOwned;
    use uuid::Version;

    const GOLDEN_UUID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const GOLDEN_JSON: &str = "\"018f3c8a-7e2b-7a10-8c4d-0123456789ab\"";

    trait TypedId:
        Copy
        + Eq
        + fmt::Debug
        + fmt::Display
        + FromStr<Err = IdParseError>
        + Serialize
        + DeserializeOwned
    {
        fn new() -> Self;
        fn as_uuid(&self) -> Uuid;
    }

    macro_rules! impl_typed_id_harness {
        ($($ty:ty),+ $(,)?) => {
            $(
                impl TypedId for $ty {
                    fn new() -> Self {
                        Self(Id::new())
                    }

                    fn as_uuid(&self) -> Uuid {
                        self.0.as_uuid()
                    }
                }
            )+
        };
    }

    impl_typed_id_harness!(
        SessionId,
        AgentId,
        GoalId,
        EvidenceId,
        EventId,
        WorkspaceViewId,
        JobId,
        LeaseId,
        KnowledgeId,
        HandoffId,
        ControlLeaseId,
        TrajectoryId,
        ProjectId,
        TurnId,
        RepoId,
        ContextItemId,
        MessageId,
        RuntimeId,
        TraceId,
        GraphId,
        NodeId,
    );

    fn assert_roundtrip<I: TypedId>(id: I) {
        assert_eq!(id.as_uuid().get_version(), Some(Version::SortRand));

        let displayed = id.to_string();
        assert_eq!(displayed, displayed.to_ascii_lowercase());
        assert_eq!(displayed.parse::<I>().expect("display parse"), id);

        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{displayed}\""));
        let decoded = serde_json::from_str::<I>(&json).expect("deserialize");
        assert_eq!(decoded, id);
    }

    #[test]
    fn every_id_type_round_trips_display_and_serde() {
        assert_roundtrip(SessionId::new());
        assert_roundtrip(AgentId::new());
        assert_roundtrip(GoalId::new());
        assert_roundtrip(EvidenceId::new());
        assert_roundtrip(EventId::new());
        assert_roundtrip(WorkspaceViewId::new());
        assert_roundtrip(JobId::new());
        assert_roundtrip(LeaseId::new());
        assert_roundtrip(KnowledgeId::new());
        assert_roundtrip(HandoffId::new());
        assert_roundtrip(ControlLeaseId::new());
        assert_roundtrip(TrajectoryId::new());
        assert_roundtrip(ProjectId::new());
        assert_roundtrip(TurnId::new());
        assert_roundtrip(RepoId::new());
        assert_roundtrip(ContextItemId::new());
        assert_roundtrip(MessageId::new());
        assert_roundtrip(RuntimeId::new());
        assert_roundtrip(TraceId::new());
    }

    #[test]
    fn session_id_new_serializes_as_lowercase_uuid_string() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        assert!(json.starts_with('"') && json.ends_with('"'));
        let inner = &json[1..json.len() - 1];
        assert_eq!(inner, inner.to_ascii_lowercase());
        assert_eq!(inner.parse::<SessionId>().expect("parse"), id);
    }

    #[test]
    fn golden_json_fixture_round_trips() {
        let id = GOLDEN_UUID.parse::<SessionId>().expect("golden parse");
        assert_eq!(serde_json::to_string(&id).expect("serialize"), GOLDEN_JSON);
        let decoded = serde_json::from_str::<SessionId>(GOLDEN_JSON).expect("deserialize");
        assert_eq!(decoded, id);
        assert_eq!(decoded.to_string(), GOLDEN_UUID);
    }

    #[test]
    fn rejects_malformed_input() {
        for sample in [
            "",
            "not-a-uuid",
            "018f3c8a7e2b7a108c4d0123456789ab",
            "018F3C8A-7E2B-7A10-8C4D-0123456789AB",
            "018f3c8a-7e2b-7a10-8c4d-0123456789ab ",
            "urn:uuid:018f3c8a-7e2b-7a10-8c4d-0123456789ab",
            "{018f3c8a-7e2b-7a10-8c4d-0123456789ab}",
            "018f3c8a-7e2b-7a10-8c4d-0123456789az",
        ] {
            assert_eq!(
                sample.parse::<SessionId>(),
                Err(IdParseError),
                "accepted {sample:?}"
            );
            assert!(serde_json::from_str::<SessionId>(&format!("\"{sample}\"")).is_err());
        }
    }

    fn property_roundtrip<I: TypedId>() -> Result<(), TestCaseError> {
        let id = I::new();
        let text = id.to_string();
        prop_assert_eq!(text.parse::<I>().ok(), Some(id));
        let json = serde_json::to_string(&id).expect("serialize");
        prop_assert_eq!(&json, &format!("\"{text}\""));
        let decoded = serde_json::from_str::<I>(&json).expect("deserialize");
        prop_assert_eq!(decoded, id);
        Ok(())
    }

    fn property_rejects<I: TypedId>(s: &str) -> Result<(), TestCaseError> {
        prop_assert_eq!(s.parse::<I>(), Err(IdParseError));
        Ok(())
    }

    proptest! {
        #[test]
        fn property_round_trip_every_id_type(_seed in any::<u64>()) {
            property_roundtrip::<SessionId>()?;
            property_roundtrip::<AgentId>()?;
            property_roundtrip::<GoalId>()?;
            property_roundtrip::<EvidenceId>()?;
            property_roundtrip::<EventId>()?;
            property_roundtrip::<WorkspaceViewId>()?;
            property_roundtrip::<JobId>()?;
            property_roundtrip::<LeaseId>()?;
            property_roundtrip::<KnowledgeId>()?;
            property_roundtrip::<HandoffId>()?;
            property_roundtrip::<ControlLeaseId>()?;
            property_roundtrip::<TrajectoryId>()?;
            property_roundtrip::<ProjectId>()?;
            property_roundtrip::<TurnId>()?;
            property_roundtrip::<RepoId>()?;
            property_roundtrip::<ContextItemId>()?;
            property_roundtrip::<MessageId>()?;
            property_roundtrip::<RuntimeId>()?;
            property_roundtrip::<TraceId>()?;
        }

        #[test]
        fn property_rejects_non_canonical_strings(s in "\\PC{0,80}") {
            let parsed = Uuid::try_parse(&s).ok();
            let canonical = parsed.map(|uuid| {
                let mut buf = [0u8; 36];
                uuid.hyphenated().encode_lower(&mut buf).to_string()
            });
            prop_assume!(canonical.as_deref() != Some(s.as_str()));
            property_rejects::<SessionId>(&s)?;
            property_rejects::<AgentId>(&s)?;
            property_rejects::<GoalId>(&s)?;
            property_rejects::<EvidenceId>(&s)?;
            property_rejects::<EventId>(&s)?;
            property_rejects::<WorkspaceViewId>(&s)?;
            property_rejects::<JobId>(&s)?;
            property_rejects::<LeaseId>(&s)?;
            property_rejects::<KnowledgeId>(&s)?;
            property_rejects::<HandoffId>(&s)?;
            property_rejects::<ControlLeaseId>(&s)?;
            property_rejects::<TrajectoryId>(&s)?;
            property_rejects::<ProjectId>(&s)?;
            property_rejects::<TurnId>(&s)?;
            property_rejects::<RepoId>(&s)?;
            property_rejects::<ContextItemId>(&s)?;
            property_rejects::<MessageId>(&s)?;
            property_rejects::<RuntimeId>(&s)?;
            property_rejects::<TraceId>(&s)?;
        }
    }
}
