// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use objects::object::{Agent, Principal, State};

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct PrincipalKey(pub Vec<u8>, pub Vec<u8>);

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct AgentKey {
    pub provider: Vec<u8>,
    pub model: Vec<u8>,
    pub session_id: Option<Vec<u8>>,
    pub segment_id: Option<Vec<u8>>,
    pub policy_id: Option<Vec<u8>>,
}

pub struct StateDictionaries {
    pub principals: Vec<PrincipalKey>,
    pub agents: Vec<AgentKey>,
    principal_indices: BTreeMap<PrincipalKey, u64>,
    agent_indices: BTreeMap<AgentKey, u64>,
}

impl StateDictionaries {
    pub fn from_states(states: &[State]) -> Self {
        let mut value = Self {
            principals: Vec::new(),
            agents: Vec::new(),
            principal_indices: BTreeMap::new(),
            agent_indices: BTreeMap::new(),
        };
        for state in states {
            value.intern_principal(&state.attribution.principal);
            if let Some(committer) = &state.committer {
                value.intern_principal(committer);
            }
            if let Some(agent) = &state.attribution.agent {
                value.intern_agent(agent);
            }
        }
        value
    }

    pub fn principal_index(&self, principal: &Principal) -> u64 {
        self.principal_indices[&principal_key(principal)]
    }

    pub fn agent_index(&self, agent: &Agent) -> u64 {
        self.agent_indices[&agent_key(agent)]
    }

    fn intern_principal(&mut self, principal: &Principal) {
        let key = principal_key(principal);
        if !self.principal_indices.contains_key(&key) {
            let index = self.principals.len() as u64;
            self.principals.push(key.clone());
            self.principal_indices.insert(key, index);
        }
    }

    fn intern_agent(&mut self, agent: &Agent) {
        let key = agent_key(agent);
        if !self.agent_indices.contains_key(&key) {
            let index = self.agents.len() as u64;
            self.agents.push(key.clone());
            self.agent_indices.insert(key, index);
        }
    }
}

fn principal_key(value: &Principal) -> PrincipalKey {
    PrincipalKey(
        value.name.as_bytes().to_vec(),
        value.email.as_bytes().to_vec(),
    )
}

fn agent_key(value: &Agent) -> AgentKey {
    AgentKey {
        provider: value.provider.as_bytes().to_vec(),
        model: value.model.as_bytes().to_vec(),
        session_id: value
            .session_id
            .as_ref()
            .map(|value| value.as_bytes().to_vec()),
        segment_id: value
            .segment_id
            .as_ref()
            .map(|value| value.as_bytes().to_vec()),
        policy_id: value
            .policy_id
            .as_ref()
            .map(|value| value.as_bytes().to_vec()),
    }
}
