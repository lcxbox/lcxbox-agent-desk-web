import { AssistantSkillsSettings } from '@/features/settings/assistant-skills-settings';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';

export default function AiAssistantSkills() {
    return <AiAssistantHubLayout>
        <AssistantSkillsSettings scope="personal" />
    </AiAssistantHubLayout>;
}
