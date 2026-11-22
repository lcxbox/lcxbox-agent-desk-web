import { useId, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import {
    AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
    AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from '@/components/ui/alert-dialog';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { getAiAgentApprovalMode, updateAiAgentApprovalMode } from '@/services/clients';
import type { AgentApprovalMode, AgentApprovalModeView } from '@/services/types';

const modeKey = (session: string | null | undefined) => ['ai-agent-approval-mode', session] as const;

export function useAgentApprovalMode(session: string | null | undefined, enabled = true) {
    return useQuery({
        queryKey: modeKey(session),
        enabled: enabled && !!session,
        queryFn: async ({ signal }) => {
            if (!session) throw new Error('Agent session unavailable');
            const response = await getAiAgentApprovalMode({ session }, { signal });
            if (!response.success || !response.data) throw new Error('Approval mode unavailable');
            return response.data;
        },
    });
}

export function subscriptionApprovalMode(mode: AgentApprovalModeView): 'manual' | 'ai_review' {
    return mode.mode === 'manual' || !mode.aiReviewAvailable ? 'manual' : 'ai_review';
}

export function AgentApprovalModePicker({ session, disabled = false }: { session: string; disabled?: boolean }) {
    const { t } = useTranslation();
    const fieldId = useId();
    const client = useQueryClient();
    const current = useAgentApprovalMode(session);
    const [confirmRevision, setConfirmRevision] = useState<number | null>(null);
    const save = useMutation({
        mutationFn: async ({ mode, expectedRevision }: { mode: AgentApprovalMode; expectedRevision: number }) => {
            const response = await updateAiAgentApprovalMode({
                session, mode, expectedRevision,
                noReviewConfirmationId: mode === 'no_review' ? v4() : null,
            });
            if (!response.success || !response.data) throw new Error('Approval mode update failed');
            return response.data;
        },
        onSuccess: data => client.setQueryData(modeKey(session), data),
        onError: () => void client.invalidateQueries({ queryKey: modeKey(session) }),
    });
    if (current.isPending) return <p role="status">{t('common.loading')}</p>;
    if (!current.data) return <div className="flex items-center gap-2">
        <p role="alert" className="text-sm text-destructive">{t('pages.aiAgent.approvalMode.loadFailed')}</p>
        <Button size="sm" variant="outline" onClick={() => void current.refetch()}>{t('pages.aiAgent.history.retry')}</Button>
    </div>;
    const { mode, revision, aiReviewAvailable } = current.data;
    return <div className="space-y-2 rounded-md border px-3 py-2 text-sm">
        <Label htmlFor={fieldId}>{t('pages.aiAgent.approvalMode.title')}</Label>
        <Select value={mode} disabled={disabled || save.isPending} onValueChange={(next: AgentApprovalMode) => {
            if (next === 'no_review') setConfirmRevision(revision);
            else save.mutate({ mode: next, expectedRevision: revision });
        }}>
            <SelectTrigger id={fieldId} className="w-full sm:w-56"><SelectValue /></SelectTrigger>
            <SelectContent>
                <SelectItem value="ai_review">{t('pages.aiAgent.approvalMode.ai_review')}</SelectItem>
                <SelectItem value="manual">{t('pages.aiAgent.approvalMode.manual')}</SelectItem>
                <SelectItem value="no_review">{t('pages.aiAgent.approvalMode.no_review')}</SelectItem>
            </SelectContent>
        </Select>
        {mode === 'ai_review' && !aiReviewAvailable && <p className="text-muted-foreground">{t('pages.aiAgent.approvalMode.manualFallback')}</p>}
        {mode === 'no_review' && <p className="text-destructive">{t('pages.aiAgent.approvalMode.directOnly')}</p>}
        {save.isError && <p role="alert" className="text-destructive">{t('pages.aiAgent.approvalMode.saveFailed')}</p>}
        <AlertDialog open={confirmRevision !== null} onOpenChange={open => { if (!open) setConfirmRevision(null); }}>
            <AlertDialogContent>
                <AlertDialogHeader>
                    <AlertDialogTitle>{t('pages.aiAgent.approvalMode.confirmTitle')}</AlertDialogTitle>
                    <AlertDialogDescription>{t('pages.aiAgent.approvalMode.risk')}</AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                    <AlertDialogCancel>{t('common.cancel')}</AlertDialogCancel>
                    <AlertDialogAction className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
                        onClick={() => {
                            if (confirmRevision !== null) save.mutate({ mode: 'no_review', expectedRevision: confirmRevision });
                        }}>{t('pages.aiAgent.approvalMode.confirm')}</AlertDialogAction>
                </AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
    </div>;
}
