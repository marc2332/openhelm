pub mod fs;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

use crate::ai::client::ToolDefinition;
use crate::config::{FsPermissions, Profile};
use opencontrol_sdk::Skill;

pub struct ToolOutput {
    pub success: bool,
    pub output: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, args: &Value, context: &ToolContext) -> Result<ToolOutput>;
}

pub struct ToolContext {
    pub fs: FsPermissions,
}

impl ToolContext {
    pub fn from_profile(profile: &Profile) -> Self {
        Self {
            fs: profile.fs.clone().unwrap_or_default(),
        }
    }
}

struct SdkToolAdapter(Box<dyn opencontrol_sdk::Tool>);

#[async_trait]
impl Tool for SdkToolAdapter {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn definition(&self) -> ToolDefinition {
        self.0.definition()
    }

    async fn execute(&self, args: &Value, _context: &ToolContext) -> Result<ToolOutput> {
        let result = self.0.execute(args).await?;
        Ok(ToolOutput {
            success: result.success,
            output: result.output,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGroup {
    Fs,
    Skill(&'static str),
}

pub struct SkillRegistry {
    skills: Vec<Box<dyn Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: vec![
                Box::new(opencontrol_github::GithubSkill),
                Box::new(opencontrol_http::HttpSkill),
            ],
        }
    }

    pub async fn build_tools_for(
        &self,
        profile: &Profile,
    ) -> Result<(Vec<Box<dyn Tool>>, HashMap<String, &'static str>)> {
        let mut tools: Vec<Box<dyn Tool>> = vec![];
        let mut name_map: HashMap<String, &'static str> = HashMap::new();

        for skill in &self.skills {
            if let Some(skill_config) = profile.permissions.skills.get(skill.name()) {
                let built = skill.build_tools(Some(skill_config)).await?;
                for sdk_tool in built {
                    name_map.insert(sdk_tool.name().to_string(), skill.name());
                    tools.push(Box::new(SdkToolAdapter(sdk_tool)));
                }
            }
        }

        Ok((tools, name_map))
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ToolRegistry {
    fs_tools: Vec<Box<dyn Tool>>,
    skill_tools: Vec<Box<dyn Tool>>,
    skill_tool_map: HashMap<String, &'static str>,
}

impl ToolRegistry {
    pub async fn for_profile(profile: &Profile, skills: &SkillRegistry) -> Result<Self> {
        let (skill_tools, skill_tool_map) = skills.build_tools_for(profile).await?;
        Ok(Self {
            fs_tools: vec![
                Box::new(fs::FsReadTool),
                Box::new(fs::FsWriteTool),
                Box::new(fs::FsListTool),
                Box::new(fs::FsMkdirTool),
            ],
            skill_tools,
            skill_tool_map,
        })
    }

    pub fn definitions_for(&self, profile: &Profile) -> Vec<ToolDefinition> {
        let fs_defs = profile
            .permissions
            .fs
            .then(|| self.fs_tools.iter().map(|tool| tool.definition()))
            .into_iter()
            .flatten();
        let skill_defs = self.skill_tools.iter().map(|tool| tool.definition());
        fs_defs.chain(skill_defs).collect()
    }

    pub fn find(&self, name: &str) -> Option<(&dyn Tool, ToolGroup)> {
        self.fs_tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| (tool.as_ref(), ToolGroup::Fs))
            .or_else(|| {
                self.skill_tools
                    .iter()
                    .find(|tool| tool.name() == name)
                    .map(|tool| {
                        let skill_name = self.skill_tool_map[name];
                        (tool.as_ref(), ToolGroup::Skill(skill_name))
                    })
            })
    }
}
