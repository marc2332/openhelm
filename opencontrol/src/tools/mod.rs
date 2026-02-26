pub mod fs;

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

use crate::ai::client::ToolDefinition;
use crate::config::{FsPermissions, Profile};
use opencontrol_sdk::Skill;

pub struct ToolOutput {
    pub success: bool,
    pub output: String,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>>;
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

impl Tool for SdkToolAdapter {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn definition(&self) -> ToolDefinition {
        self.0.definition()
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let result = self.0.execute(args).await?;
            Ok(ToolOutput {
                success: result.success,
                output: result.output,
            })
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

    pub fn build_tools_for(
        &self,
        profile: &Profile,
    ) -> Result<(Vec<Box<dyn Tool>>, HashMap<String, &'static str>)> {
        let mut tools: Vec<Box<dyn Tool>> = vec![];
        let mut name_map: HashMap<String, &'static str> = HashMap::new();

        for skill in &self.skills {
            if let Some(skill_config) = profile.permissions.skills.get(skill.name()) {
                let built = skill.build_tools(Some(skill_config))?;
                for t in built {
                    name_map.insert(t.name().to_string(), skill.name());
                    tools.push(Box::new(SdkToolAdapter(t)));
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
    pub fn for_profile(profile: &Profile, skills: &SkillRegistry) -> Result<Self> {
        let (skill_tools, skill_tool_map) = skills.build_tools_for(profile)?;
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
        let mut defs = vec![];
        if profile.permissions.fs {
            defs.extend(self.fs_tools.iter().map(|t| t.definition()));
        }
        defs.extend(self.skill_tools.iter().map(|t| t.definition()));
        defs
    }

    pub fn find(&self, name: &str) -> Option<(&dyn Tool, ToolGroup)> {
        if let Some(t) = self.fs_tools.iter().find(|t| t.name() == name) {
            return Some((t.as_ref(), ToolGroup::Fs));
        }
        if let Some(t) = self.skill_tools.iter().find(|t| t.name() == name) {
            let skill_name = self.skill_tool_map[name];
            return Some((t.as_ref(), ToolGroup::Skill(skill_name)));
        }
        None
    }
}
