import { AgentSkillsSettings } from '@/features/settings/agent-skills-settings';
import { AiAgentHubLayout } from './ai-agent-hub-layout';

export default function AiAgentSkills() {
    return <AiAgentHubLayout>
        <AgentSkillsSettings scope="personal" />
    </AiAgentHubLayout>;
}
