import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Activity, Bell, BookOpen, Brain, CalendarClock, ListTodo, MessagesSquare, Search, SlidersHorizontal } from 'lucide-react';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { AiAgentIcon } from '@/components/ai-agent-icon';

export function AiAgentOverview({ showPersonalConversation = true, showWorkbench = true }: {
    showPersonalConversation?: boolean;
    showWorkbench?: boolean;
}) {
    const { t } = useTranslation();

    return <div className="mx-auto w-full max-w-6xl p-6">
        <h1 className="mb-4 text-2xl font-bold tracking-tight">{t('pages.aiAgent.title')}</h1>
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
            {showPersonalConversation && <Link to="/ai-agent/conversation" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <MessagesSquare className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.conversations.personal')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.overview.personalConversationDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>}
            {showWorkbench && <Link to="/ai-agent/workbench" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <ListTodo className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.workbench.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.workbench.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>}
            <Link to="/ai-agent/conversations" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <AiAgentIcon className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.overview.conversationsTitle')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.overview.conversationsDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-agent/attention" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Bell className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgentAttention.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgentAttention.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/schedules" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <CalendarClock className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('schedules.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.overview.schedulesDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-agent/personalization" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <SlidersHorizontal className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.personalization.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.personalization.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-agent/skills" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <BookOpen className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.agentSkills.personal.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.agentSkills.personal.scope')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-agent/processing" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Activity className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.processing.title')}</CardTitle>
                        </div>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-agent/history" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Search className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.historySearch.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.historySearch.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-agent/memories" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Brain className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAgent.memory.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAgent.memory.overviewDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
        </div>
    </div>;
}
