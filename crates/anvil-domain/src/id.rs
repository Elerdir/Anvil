//! Typované identifikátory.
//!
//! `ConversationId` a `MessageId` jsou obalené `Uuid`, aby je nešlo zaměnit —
//! obojí je jinak jen 16 bajtů a překladač by prohození nezachytil.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! typed_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

typed_id!(ConversationId, "Identifikátor konverzace.");
typed_id!(MessageId, "Identifikátor jedné zprávy v konverzaci.");

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn nove_id_jsou_ruzna() {
        assert_ne!(ConversationId::new(), ConversationId::new());
    }

    #[test]
    fn id_prezije_kolecko_pres_text() {
        let id = MessageId::new();
        assert_eq!(MessageId::from_str(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn id_se_serializuje_jako_holy_retezec() {
        // `transparent` — v JSONu nemá být objekt s jedním polem.
        let id = ConversationId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
    }
}
