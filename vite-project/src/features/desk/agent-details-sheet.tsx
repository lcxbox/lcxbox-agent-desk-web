import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export type AgentPanelId = 'details' | 'capabilities' | 'context' | 'connection' | 'observation';

export function AgentDetailsSheet({ panel, onPanelChange, sections }: {
    panel: AgentPanelId | null;
    onPanelChange: (panel: AgentPanelId | null) => void;
    sections: Partial<Record<AgentPanelId, ReactNode>>;
}) {
    const { t } = useTranslation();
    return (
        <Sheet open={panel !== null} onOpenChange={(open) => { if (!open) onPanelChange(null); }}>
            <SheetContent aria-describedby={undefined} className="flex w-full flex-col overflow-hidden sm:max-w-xl">
                <SheetHeader className="pr-8 text-left">
                    <SheetTitle>{t(`pages.aiAgent.workspace.${panel ?? 'details'}`)}</SheetTitle>
                </SheetHeader>
                <nav className="flex flex-wrap gap-1 border-b py-3" aria-label={t('pages.aiAgent.workspace.details')}>
                    {(['details', 'capabilities', 'context', 'connection', 'observation'] as const).filter(id => id in sections).map((id) => (
                        <Button key={id} type="button" size="sm" variant={panel === id ? 'secondary' : 'ghost'}
                            aria-current={panel === id ? 'page' : undefined} onClick={() => onPanelChange(id)}>
                            {t(`pages.aiAgent.workspace.${id}`)}
                        </Button>
                    ))}
                </nav>
                <div className="min-h-0 flex-1 space-y-4 overflow-y-auto overscroll-contain py-4 [overflow-wrap:anywhere]">
                    {panel && sections[panel]}
                </div>
            </SheetContent>
        </Sheet>
    );
}
