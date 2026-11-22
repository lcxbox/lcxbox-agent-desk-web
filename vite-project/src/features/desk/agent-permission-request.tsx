import { permissionToolLabel, permissionEffectLabel, permissionResourceLabel, permissionOperationLabel, permissionReasonLabel, isReusableWebHost } from './agent-permission-labels';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import type { PermissionDecisionBody, PermissionRequestDto } from '@/services/types';
import { OutputConfirmationCard, validOutputReview, isOutputInput, outputApprovalBlocked } from './ai-agent-output-confirmation';
import { AgentPermissionDisclosure } from './agent-permission-disclosure';
import { CommandConfirmationCard, validCommandReview } from './ai-agent-command';
import { LaunchConfirmationCard, validLaunchReview } from './ai-agent-launch';
import { TextFileConfirmationCard, validTextFileReview, fileApprovalBlocked } from './ai-agent-file-confirmation';
import { CommandRestoreConfirmationCard, COMMAND_RESTORE_TOOL, validCommandRestoreReview } from './agent-command-changes';
import { AgentInteractiveCommand, interactiveCommandItem, type InteractiveCommandContext } from './agent-interactive-command';
import {
    defaultApprovedItemIds, hasNativeUiAuthority, needsApplicationScope, permissionDecisionItems, permissionDenyItems,
    permissionItemApprovalBlocked, type PermissionItemEdit,
} from './agent-permission-decision';

function formatByteCount(value: number) {
    if (value < 1024) return `${value} B`;
    if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
    return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
}

export function AgentPermissionRequest({ request, canDecide, disabled = false, busy = false, waitingForTurn = false, interactive, onDecide }: {
    request: PermissionRequestDto;
    canDecide: boolean;
    disabled?: boolean;
    busy?: boolean;
    waitingForTurn?: boolean;
    /** Where an interactive command's terminal can be opened; absent where it cannot. */
    interactive?: InteractiveCommandContext;
    onDecide: (request: PermissionRequestDto, items: PermissionDecisionBody['items'], carrierId?: string) => Promise<boolean>;
}) {
    const interactiveItem = interactiveCommandItem(request);
    const { t } = useTranslation();
    const decision = request.decision;
    const [permissionSelections, setPermissionSelections] = useState<Record<string, string[]>>({});
    const [permissionEdits, setPermissionEdits] = useState<
        Record<string, Record<string, PermissionItemEdit>>
    >({});
    const updatePermissionItemEdit = (
        requestId: string,
        itemId: string,
        update: (current: PermissionItemEdit) => PermissionItemEdit,
    ) => {
        setPermissionEdits((current) => ({
            ...current,
            [requestId]: {
                ...current[requestId],
                [itemId]: update(current[requestId]?.[itemId] ?? {}),
            },
        }));
    };

    const togglePermissionScope = (
        requestId: string,
        itemId: string,
        field: 'resourceScope' | 'operationScope',
        value: string,
        defaults: string[],
    ) => {
        updatePermissionItemEdit(requestId, itemId, (current) => {
            const values = current[field] ?? defaults;
            return {
                ...current,
                [field]: values.includes(value)
                    ? values.filter((entry) => entry !== value)
                    : [...values, value],
            };
        });
    };

    const togglePermissionItem = (
        requestId: string,
        defaultItemIds: string[],
        itemId: string,
    ) => {
        setPermissionSelections((current) => {
            const selected = current[requestId] ?? defaultItemIds;
            return {
                ...current,
                [requestId]: selected.includes(itemId)
                    ? selected.filter((id) => id !== itemId)
                    : [...selected, itemId],
            };
        });
    };


    return <AgentPermissionDisclosure state={request.state} tools={request.items.map((item) => item.toolName)}>
        <fieldset disabled={disabled || busy || waitingForTurn} className="min-w-0 space-y-3">
            {waitingForTurn && request.state === 'pending' && <p role="status" className="text-xs text-muted-foreground">
                {t('pages.aiAgent.permissionWaitingForTurn')}
            </p>}
            {decision && <p className="text-xs text-muted-foreground">
                {t(`pages.aiAgent.permissionDecisionSource.${decision.source}`)}
            </p>}
            {request.items.some((item) => ['execute_ui_actions', 'send_background_input', 'send_raw_input'].includes(item.toolName))
                && ['inspect_desktop_session', 'inspect_desktop_ui'].every((name) => request.items.some((item) => item.toolName === name)) && (
                <p className="text-xs text-muted-foreground">{t('pages.aiAgent.permissionIncludedDesktopReads')}</p>
            )}
            <div className="space-y-2">
                {request.items.map((item) => {
                    const decidedItem = decision?.items.find((entry) => entry.itemId === item.itemId);
                    const reason = item.itemId === `included-${item.toolName}`
                        && ['inspect_desktop_session', 'inspect_desktop_ui'].includes(item.toolName)
                        ? t('pages.aiAgent.permissionIncludedDesktopReadReason') : permissionReasonLabel(t, item.reason);
                    const defaultItemIds = defaultApprovedItemIds(request);
                    const selected = permissionSelections[request.requestId]
                        ?? defaultItemIds;
                    const approved = selected.includes(item.itemId);
                    const isExternalSend = item.expectedEffect === 'send_external';
                    const reusableWebHost = isReusableWebHost(item);
                    const sendConfirmation = item.externalSendConfirmation;
                    const commandConfirmation = item.commandConfirmation;
                    const outputBlocked = outputApprovalBlocked(item);
                    const commandBlocked = item.toolName === 'exec_command' && !validCommandReview(commandConfirmation);
                    const launchBlocked = item.toolName === 'launch_application' && !validLaunchReview(item.launchConfirmation);
                    const approvalBlocked = permissionItemApprovalBlocked(item);
                    const edit = permissionEdits[request.requestId]?.[item.itemId]
                        ?? {};
                    const resourceScope = edit.resourceScope
                        ?? item.resourceScope;
                    const operationScope = edit.operationScope
                        ?? item.operationScope;
                    const exportDestinationIndexes = edit.exportDestinationIndexes
                        ?? item.exportDestinations.map((_, index) => index);
                    return (
                        <div key={item.itemId} className="flex items-start gap-3 rounded border bg-background px-3 py-2">
                            {canDecide
                                && request.state === 'pending' && (
                                <Checkbox
                                    className="mt-0.5"
                                    checked={approved}
                                    disabled={approvalBlocked}
                                    aria-label={t('pages.aiAgent.permissionItemToggle', { reason })}
                                    onCheckedChange={() => togglePermissionItem(
                                        request.requestId,
                                        defaultItemIds,
                                        item.itemId,
                                    )}
                                />
                            )}
                            <div>
                                <p className="text-sm font-medium">{reason}</p>
                                {decidedItem && <div className="mt-2 space-y-1 text-xs">
                                    <p className={decidedItem.approved ? 'text-emerald-700 dark:text-emerald-300' : 'text-red-700 dark:text-red-300'}>
                                        {t(`pages.aiAgent.permissionItemOutcome.${decidedItem.outcome}`)} · {t(`pages.aiAgent.permissionDecisionSource.${decidedItem.source}`)}
                                    </p>
                                    {decidedItem.reason && <p className="whitespace-pre-wrap break-words text-muted-foreground">
                                        {t('pages.aiAgent.permissionDecisionReason')}: {decidedItem.source === 'system' ? t(`pages.aiAgent.permissionSystemReason.${decidedItem.reasonCode}`, { defaultValue: decidedItem.reason }) : decidedItem.reason}
                                    </p>}
                                </div>}
                                <p className="mt-1 break-all text-xs text-muted-foreground">
                                    {permissionToolLabel(t, item.toolName)} · {permissionEffectLabel(t, item.expectedEffect, item.toolName)}
                                    {item.selectedReadDeviceRef && <> · {t('pages.aiAgent.permissionReadTarget', { target: item.selectedReadDeviceRef })}</>}
                                </p>
                                {item.applicationScope && (
                                    <div data-testid="application-ui-scope" className="mt-3 space-y-1 rounded-md border p-3 text-xs">
                                        <p className="font-medium">{t('pages.aiAgent.applicationUiScopeTitle', { name: item.applicationScope.application_name })}</p>
                                        <p>{t('pages.aiAgent.applicationUiScopeDescription')}</p>
                                        <p>{t('pages.aiAgent.applicationUiScopeLifetime')}</p>
                                        <p>{item.applicationScope.actions.map((action) => t(`pages.aiAgent.uiAction_${action}`)).join(' · ')}</p>
                                    </div>
                                )}
                                {validOutputReview(item.waylandOutputConfirmation) && <OutputConfirmationCard value={item.waylandOutputConfirmation} />}
                                {validCommandReview(commandConfirmation) && <CommandConfirmationCard value={commandConfirmation} />}
                                {validLaunchReview(item.launchConfirmation) && <LaunchConfirmationCard value={item.launchConfirmation} />}
                                {validTextFileReview(item.textFileConfirmation) && <TextFileConfirmationCard value={item.textFileConfirmation} />}
                                {validCommandRestoreReview(item.commandRestoreConfirmation) && <CommandRestoreConfirmationCard value={item.commandRestoreConfirmation} />}
                                {sendConfirmation && (
                                    <div data-testid="external-send-confirmation" className="mt-3 space-y-2 rounded-md border border-red-500/50 bg-red-500/5 p-3 text-xs">
                                        <p className="flex items-center gap-2 font-semibold text-red-700 dark:text-red-300">
                                            <AlertTriangle className="h-4 w-4" />
                                            {t('pages.aiAgent.externalSendConfirmationTitle')}
                                        </p>
                                        <p>{t('pages.aiAgent.externalSendOneShotWarning')}</p>
                                        <dl className="grid gap-x-3 gap-y-1 sm:grid-cols-[max-content_1fr]">
                                            <dt className="font-medium">{t('pages.aiAgent.externalSendAccount')}</dt>
                                            <dd className="break-all">{sendConfirmation.accountId}</dd>
                                            <dt className="font-medium">{t('pages.aiAgent.externalSendDestination')}</dt>
                                            <dd className="break-all">{sendConfirmation.destination}</dd>
                                            {sendConfirmation.subject != null && (
                                                <>
                                                    <dt className="font-medium">{t('pages.aiAgent.externalSendSubject')}</dt>
                                                    <dd className="break-words">{sendConfirmation.subject}</dd>
                                                </>
                                            )}
                                            <dt className="font-medium">{t('pages.aiAgent.externalSendBody')}</dt>
                                            <dd>{formatByteCount(sendConfirmation.bodySizeBytes)}</dd>
                                        </dl>
                                        <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-background p-2">
                                            {sendConfirmation.bodyPlainText}
                                        </pre>
                                        {sendConfirmation.attachments.length > 0 && (
                                            <div>
                                                <p className="font-medium">{t('pages.aiAgent.externalSendAttachments')}</p>
                                                <ul className="list-disc pl-5">
                                                    {sendConfirmation.attachments.map((attachment) => (
                                                        <li key={`${attachment.fileName}:${attachment.sizeBytes}`} className="break-all">
                                                            {attachment.fileName} · {formatByteCount(attachment.sizeBytes)}
                                                        </li>
                                                    ))}
                                                </ul>
                                            </div>
                                        )}
                                    </div>
                                )}
                                {approvalBlocked && (
                                    <p className="mt-2 text-xs font-medium text-red-700 dark:text-red-300">
                                        {t(outputBlocked ? 'pages.aiAgent.outputConfirmMissing' : needsApplicationScope(item.toolName) && !hasNativeUiAuthority(item) ? 'pages.aiAgent.applicationUiScopeMissing' : fileApprovalBlocked(item) ? (item.toolName === COMMAND_RESTORE_TOOL ? 'pages.aiAgent.commandRestore.missing' : 'pages.aiAgent.fileConfirmMissing')
                                            : launchBlocked ? 'pages.aiAgent.launchSummaryMissing' : commandBlocked ? 'pages.aiAgent.commandSummaryMissing' : 'pages.aiAgent.externalSendSummaryMissing')}
                                    </p>
                                )}
                                {!item.applicationScope && (item.resourceScope.length > 0 || item.operationScope.length > 0) && (
                                    <p className="mt-1 break-all text-xs text-muted-foreground">
                                        {[...item.resourceScope.map((scope) => permissionResourceLabel(t, scope)), ...item.operationScope.map((scope) => permissionOperationLabel(t, scope, item.toolName))].join(' · ')}
                                    </p>
                                )}
                                {canDecide
                                    && request.state === 'pending'
                                    && approved && (
                                    <div className="mt-3 space-y-3 border-t pt-3">
                                        {item.resourceScope.length > 0 && (
                                            <div className="space-y-1">
                                                <p className="text-xs font-medium">
                                                    {t('pages.aiAgent.permissionResourceScope')}
                                                </p>
                                                {item.resourceScope.map((scope) => (
                                                    <label key={scope} className="flex items-center gap-2 text-xs">
                                                        <Checkbox
                                                            checked={resourceScope.includes(scope)}
                                                            onCheckedChange={() => togglePermissionScope(
                                                                request.requestId,
                                                                item.itemId,
                                                                'resourceScope',
                                                                scope,
                                                                item.resourceScope,
                                                            )}
                                                        />
                                                        <span className="break-all">{permissionResourceLabel(t, scope, item.applicationScope?.application_name)}</span>
                                                    </label>
                                                ))}
                                            </div>
                                        )}
                                        {item.applicationScope && <p className="text-xs text-muted-foreground">{t('pages.aiAgent.batchGrantUses')}</p>}
                                        {item.operationScope.length > 0 && (
                                            <div className="space-y-1">
                                                <p className="text-xs font-medium">
                                                    {t('pages.aiAgent.permissionOperationScope')}
                                                </p>
                                                {item.operationScope.map((scope) => (
                                                    <label key={scope} className="flex items-center gap-2 text-xs">
                                                        <Checkbox
                                                            checked={operationScope.includes(scope)}
                                                            onCheckedChange={() => togglePermissionScope(
                                                                request.requestId,
                                                                item.itemId,
                                                                'operationScope',
                                                                scope,
                                                                item.operationScope,
                                                            )}
                                                        />
                                                        <span className="break-all">{permissionOperationLabel(t, scope, item.toolName)}</span>
                                                    </label>
                                                ))}
                                            </div>
                                        )}
                                        {item.exportDestinations.length > 0 && (
                                            <div className="space-y-1">
                                                <p className="text-xs font-medium">
                                                    {t('pages.aiAgent.permissionDestinations')}
                                                </p>
                                                {item.exportDestinations.map((destination, index) => (
                                                    <label key={JSON.stringify(destination)} className="flex items-center gap-2 text-xs">
                                                        <Checkbox
                                                            checked={exportDestinationIndexes.includes(index)}
                                                            onCheckedChange={() => updatePermissionItemEdit(
                                                                request.requestId,
                                                                item.itemId,
                                                                (current) => {
                                                                    const indexes = current.exportDestinationIndexes
                                                                        ?? item.exportDestinations.map((_, currentIndex) => currentIndex);
                                                                    return {
                                                                        ...current,
                                                                        exportDestinationIndexes: indexes.includes(index)
                                                                            ? indexes.filter((entry) => entry !== index)
                                                                            : [...indexes, index],
                                                                    };
                                                                },
                                                            )}
                                                        />
                                                        <span className="break-all">{JSON.stringify(destination)}</span>
                                                    </label>
                                                ))}
                                            </div>
                                        )}
                                        <div className="grid gap-3 sm:grid-cols-2">
                                            <label className="space-y-1 text-xs">
                                                <span>{t('pages.aiAgent.permissionTtlSeconds')}</span>
                                                <Input
                                                    type="number"
                                                    min={1}
                                                    max={item.suggestedTtlSeconds}
                                                    value={edit.ttlSeconds ?? item.suggestedTtlSeconds}
                                                    onChange={(event) => updatePermissionItemEdit(
                                                        request.requestId,
                                                        item.itemId,
                                                        (current) => ({
                                                            ...current,
                                                            ttlSeconds: Math.max(1, Math.min(
                                                                item.suggestedTtlSeconds,
                                                                Number(event.target.value) || 1,
                                                            )),
                                                        }),
                                                    )}
                                                />
                                            </label>
                                            {reusableWebHost ? <p className="self-center text-xs text-muted-foreground">
                                                {t('pages.aiAgent.webHostReusableUntilExpiry')}
                                            </p> : <label className="space-y-1 text-xs">
                                                <span>{t('pages.aiAgent.permissionMaxUses')}</span>
                                                <Input
                                                    type="number"
                                                    min={1}
                                                    max={item.suggestedMaxUses}
                                                    value={isExternalSend || isOutputInput(item) ? 1 : (edit.maxUses ?? item.suggestedMaxUses)}
                                                    disabled={isExternalSend || isOutputInput(item)}
                                                    onChange={(event) => updatePermissionItemEdit(
                                                        request.requestId,
                                                        item.itemId,
                                                        (current) => ({
                                                            ...current,
                                                            maxUses: Math.max(1, Math.min(
                                                                item.suggestedMaxUses,
                                                                Number(event.target.value) || 1,
                                                            )),
                                                        }),
                                                    )}
                                                />
                                            </label>}
                                        </div>
                                    </div>
                                )}
                            </div>
                        </div>
                    );
                })}
            </div>
            {interactiveItem && (interactive ? (
                <AgentInteractiveCommand request={request} item={interactiveItem} context={interactive}
                    canDecide={canDecide} disabled={disabled || busy || waitingForTurn}
                    onApprove={(carrierId) => onDecide(request, [{
                        itemId: interactiveItem.itemId,
                        decision: 'approve',
                        resource_scope: interactiveItem.resourceScope,
                        operation_scope: interactiveItem.operationScope,
                        export_destinations: [],
                        ttl_seconds: interactiveItem.suggestedTtlSeconds,
                        max_uses: 1,
                    }], carrierId)} />
            ) : request.state === 'pending' && (
                <p className="text-xs text-muted-foreground">{t('pages.aiAgent.interactiveCommand.unsupportedHere')}</p>
            ))}
            {canDecide
                && request.state === 'pending' && (
                <div className="space-y-2">
                    {!interactiveItem && <p className="text-xs text-muted-foreground">
                        {t('pages.aiAgent.permissionSelectionDescription')}
                    </p>}
                    <div className="flex flex-wrap gap-2">
                    {!interactiveItem && <Button
                        type="button"
                        size="sm"
                        className="gap-1.5 px-2.5"
                        disabled={disabled || busy || waitingForTurn}
                        onClick={() => void onDecide(request, permissionDecisionItems(
                            request,
                            permissionSelections[request.requestId] ?? defaultApprovedItemIds(request),
                            permissionEdits[request.requestId],
                        ))}
                    >
                        <Check className="h-4 w-4" />
                        {t('pages.aiAgent.permissionSubmitSelection')}
                    </Button>}
                    <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        className="gap-1.5 px-2.5"
                        disabled={disabled || busy || waitingForTurn}
                        onClick={() => void onDecide(request, permissionDenyItems(request))}
                    >
                        <X className="h-4 w-4" />
                        {t('pages.aiAgent.permissionDeny')}
                    </Button>
                    </div>
                </div>
            )}
            {request.state === 'needs_revalidation' && (
                <p className="text-xs text-amber-700 dark:text-amber-300">
                    {t('pages.aiAgent.permissionNeedsRevalidation')}
                </p>
            )}
        </fieldset>
    </AgentPermissionDisclosure>;
}
