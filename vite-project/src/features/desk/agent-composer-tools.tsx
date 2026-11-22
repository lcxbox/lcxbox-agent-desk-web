import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { CalendarClock, ClipboardList, ListTree, FolderKey, ListTodo } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AgentComposerTools({ meter, onDetails, onPermissionHistory, onDirectories, onSchedules, onTasks, runningTaskCount = 0 }: {
    meter: ReactNode;
    onSchedules?: () => void;
    onTasks?: () => void;
    runningTaskCount?: number;
    onDetails: () => void;
    onPermissionHistory: () => void;
    onDirectories?: () => void;
}) {
    const { t } = useTranslation();
    const taskLabel = `${t('pages.aiAgent.tasks.title')}${runningTaskCount > 0 ? ` (${runningTaskCount})` : ''}`;
    return <div className="flex min-w-0 flex-wrap items-center gap-1" data-testid="agent-composer-tools">
        {meter}
        <Button type="button" size="sm" variant="ghost" className="agent-action" title={t('pages.aiAgent.workspace.details')}
            aria-label={t('pages.aiAgent.workspace.details')} aria-haspopup="dialog" onClick={onDetails}>
            <ListTree className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="agent-action-label">{t('pages.aiAgent.workspace.details')}</span>
        </Button>
        <Button type="button" size="sm" variant="ghost" className="agent-action" title={t('pages.aiAgent.permissionHistory')}
            aria-label={t('pages.aiAgent.permissionHistory')} aria-haspopup="dialog" onClick={onPermissionHistory}>
            <ClipboardList className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="agent-action-label">{t('pages.aiAgent.permissionHistory')}</span>
        </Button>
        {onTasks && <Button type="button" size="sm" variant="ghost" className="agent-action" title={taskLabel}
            aria-label={taskLabel} aria-haspopup="dialog" onClick={onTasks}>
            <ListTodo className="h-4 w-4 shrink-0" aria-hidden="true" />
            <span className="agent-action-label">{taskLabel}</span>
        </Button>}
        {onDirectories && <Button type="button" size="sm" variant="ghost" className="agent-action" title={t('pages.aiAgent.directories.title')}
            aria-label={t('pages.aiAgent.directories.title')} aria-haspopup="dialog" onClick={onDirectories}>
            <FolderKey className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="agent-action-label">{t('pages.aiAgent.directories.title')}</span>
        </Button>}
        {onSchedules && <Button type="button" size="sm" variant="ghost" className="agent-action" onClick={onSchedules}
            aria-label={t('pages.aiAgent.schedules.title')} title={t('pages.aiAgent.schedules.title')} aria-haspopup="dialog">
            <CalendarClock className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="agent-action-label">{t('pages.aiAgent.schedules.title')}</span>
        </Button>}
    </div>;
}
