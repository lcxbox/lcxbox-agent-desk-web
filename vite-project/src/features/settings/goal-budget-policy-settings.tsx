import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Switch } from '@/components/ui/switch';
import { Input } from '@/components/ui/input';

export type GoalBudgetLimits = {
    activeTimeMs: number | null;
    deadlineMs: number | null;
    modelTokens: number | null;
    modelCalls: number | null;
    toolCalls: number | null;
    slices: number | null;
    stalledSlices: number | null;
};

export type GoalBudgetPolicy = {
    schemaVersion: number;
    revision: number;
    limits: GoalBudgetLimits;
    /** How long a goal may wait for an unavailable device; always enforced. */
    deviceUnavailableMaxMs: number;
    ownerQuestionTimeoutMs: number;
};

const HOUR_MS = 3_600_000;
const DEVICE_UNAVAILABLE_MIN_HOURS = 1;
const DEVICE_UNAVAILABLE_MAX_HOURS = 720;

function validDeviceUnavailableMax(value: number): boolean {
    return Number.isSafeInteger(value)
        && value >= DEVICE_UNAVAILABLE_MIN_HOURS * HOUR_MS
        && value <= DEVICE_UNAVAILABLE_MAX_HOURS * HOUR_MS;
}

const fields = [
    { key: 'activeTimeMs', label: 'goalBudgetActiveHours', scale: 3_600_000, maximum: 24, defaultValue: 2 },
    { key: 'deadlineMs', label: 'goalBudgetDeadlineDays', scale: 86_400_000, maximum: 30, defaultValue: 7 },
    { key: 'modelTokens', label: 'goalBudgetTokens', scale: 1, maximum: 10_000_000, defaultValue: 10_000_000 },
    { key: 'modelCalls', label: 'goalBudgetModelCalls', scale: 1, maximum: 1_000, defaultValue: 160 },
    { key: 'toolCalls', label: 'goalBudgetToolCalls', scale: 1, maximum: 2_000, defaultValue: 200 },
    { key: 'slices', label: 'goalBudgetSlices', scale: 1, maximum: 200, defaultValue: 20 },
    { key: 'stalledSlices', label: 'goalBudgetStalledSlices', scale: 1, maximum: 10, defaultValue: 3 },
] as const;

function validPolicy(value: GoalBudgetPolicy): boolean {
    return value.schemaVersion === 2
        && Number.isSafeInteger(value.ownerQuestionTimeoutMs) && value.ownerQuestionTimeoutMs > 0 && value.ownerQuestionTimeoutMs % 1000 === 0 && Number.isSafeInteger(value.revision)
        && validDeviceUnavailableMax(value.deviceUnavailableMaxMs)
        && fields.every(({ key, scale, maximum }) => value.limits[key] === null
            || (Number.isSafeInteger(value.limits[key]) && value.limits[key]! >= scale
                && value.limits[key]! <= scale * maximum));
}

export async function fetchGoalBudgetPolicy(path = '/api/my/ai-agent-session/goal/budget-policy'): Promise<GoalBudgetPolicy> {
    const response = await fetch(path, { credentials: 'include', cache: 'no-store' });
    const body = await response.json();
    if (!response.ok || !body?.success || !body.data || !validPolicy(body.data)) {
        throw new Error(body?.message ?? 'Goal budget policy unavailable');
    }
    return body.data as GoalBudgetPolicy;
}

export default function GoalBudgetPolicySettings() {
    const { t } = useTranslation();
    const [current, setCurrent] = useState<GoalBudgetPolicy | null>(null);
    const [draft, setDraft] = useState<GoalBudgetLimits | null>(null);
    const [input, setInput] = useState<Record<keyof GoalBudgetLimits, string> | null>(null);
    const [questionSeconds, setQuestionSeconds] = useState('');
    const [deviceHours, setDeviceHours] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [saved, setSaved] = useState(false);
    const adopt = (policy: GoalBudgetPolicy) => {
        setCurrent(policy);
        setDraft(policy.limits);
        setInput(Object.fromEntries(fields.map(({ key, scale, defaultValue }) => [key,
            String(policy.limits[key] === null ? defaultValue : policy.limits[key]! / scale)])) as Record<keyof GoalBudgetLimits, string>);
        setQuestionSeconds(String(policy.ownerQuestionTimeoutMs / 1000));
        setDeviceHours(String(policy.deviceUnavailableMaxMs / HOUR_MS));
    };
    const reload = async () => {
        setBusy(true); setError(''); setSaved(false);
        try { adopt(await fetchGoalBudgetPolicy('/api/admin/system/goal-budget-policy')); }
        catch (cause) { setCurrent(null); setError(cause instanceof Error ? cause.message : 'Load failed'); }
        finally { setBusy(false); }
    };
    useEffect(() => { void reload(); }, []);
    const next = draft && input ? Object.fromEntries(fields.map(({ key, scale }) => {
        const number = Number(input[key]);
        return [key, draft[key] === null ? null : number * scale];
    })) as GoalBudgetLimits : null;
    const nextDeviceMax = Number(deviceHours) * HOUR_MS;
    const questionTimeout = Number(questionSeconds) * 1000;
    const valid = Number.isSafeInteger(Number(questionSeconds)) && questionTimeout > 0 && Number.isSafeInteger(questionTimeout) && next !== null && validDeviceUnavailableMax(nextDeviceMax) && fields.every(({ key, scale, maximum }) =>
        next[key] === null || (Number.isSafeInteger(next[key]) && next[key]! >= scale && next[key]! <= scale * maximum));
    const dirty = !!current && !!next && (fields.some(({ key }) => current.limits[key] !== next[key])
        || current.deviceUnavailableMaxMs !== nextDeviceMax || current.ownerQuestionTimeoutMs !== questionTimeout);
    const save = async () => {
        if (!current || !next || !valid || !dirty || busy) return;
        setBusy(true); setError(''); setSaved(false);
        try {
            const response = await fetch('/api/admin/system/goal-budget-policy', {
                method: 'PUT', credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    expectedRevision: current.revision,
                    limits: next,
                    deviceUnavailableMaxMs: nextDeviceMax,
                    ownerQuestionTimeoutMs: questionTimeout,
                }),
            });
            const body = await response.json();
            if (!response.ok || !body?.success || !validPolicy(body.data)) throw new Error(body?.message ?? 'Save failed');
            adopt(body.data);
            setSaved(true);
        } catch (cause) {
            setCurrent(null);
            setError(cause instanceof Error ? cause.message : 'Save failed');
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.aiAgent.goalBudgetPolicyTitle')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAgent.goalBudgetPolicyDescription')}</p>
            {fields.map(({ key, label, scale, maximum }) => <div key={key} className="flex flex-wrap items-center gap-3">
                <label className="flex min-w-56 items-center gap-2">
                    {key === 'activeTimeMs' || key === 'deadlineMs'
                        ? <Switch checked={draft?.[key] !== null && !!draft} disabled={!draft || busy}
                            aria-label={t(`pages.aiAgent.${label}`)}
                            onCheckedChange={(checked) => setDraft(previous => previous && ({
                                ...previous, [key]: checked ? Number(input?.[key] ?? 1) * scale : null,
                            }))} />
                        : <Checkbox checked={draft?.[key] !== null && !!draft} disabled={!draft || busy}
                            onCheckedChange={(checked) => setDraft(previous => previous && ({
                                ...previous, [key]: checked === true ? Number(input?.[key] ?? 1) * scale : null,
                            }))} />}
                    <span>{t(`pages.aiAgent.${label}`)}</span>
                </label>
                <Input className="w-32" type="number" min={1} max={maximum} step={1}
                    value={input?.[key] ?? ''} disabled={!draft || draft[key] === null || busy}
                    onChange={(event) => setInput(previous => previous && ({ ...previous, [key]: event.target.value }))} />
                {draft?.[key] === null && <span className="text-sm text-muted-foreground">{t('pages.aiAgent.goalBudgetDisabled')}</span>}
            </div>)}
            <div className="space-y-1 border-t pt-4">
                <div className="flex flex-wrap items-center gap-3">
                    <span className="min-w-56">{t('pages.aiAgent.goalBudgetDeviceUnavailableHours')}</span>
                    <Input className="w-32" type="number" min={DEVICE_UNAVAILABLE_MIN_HOURS}
                        max={DEVICE_UNAVAILABLE_MAX_HOURS} step={1} value={deviceHours}
                        disabled={!draft || busy} aria-label={t('pages.aiAgent.goalBudgetDeviceUnavailableHours')}
                        onChange={(event) => setDeviceHours(event.target.value)} />
                </div>
                <p className="text-sm text-muted-foreground">{t('pages.aiAgent.goalBudgetDeviceUnavailableDescription')}</p>
            </div>
            <label className="flex flex-wrap items-center gap-3">
                <span className="min-w-56">{t('pages.aiAgent.goalQuestionTimeoutSeconds')}</span>
                <Input className="w-32" type="number" min={1} step={1} value={questionSeconds}
                    disabled={!draft || busy} onChange={event => setQuestionSeconds(event.target.value)} />
            </label>
            {error && <p role="alert" className="text-destructive">{error}</p>}
            {draft && !valid && <p role="alert" className="text-destructive">{t('pages.aiAgent.goalBudgetInvalid')}</p>}
            {saved && <p role="status">{t('pages.aiAgent.goalBudgetSaved')}</p>}
            <div className="flex gap-2">
                <Button onClick={() => void save()} disabled={!current || busy || !valid || !dirty}>{t('pages.aiAgent.goalBudgetSave')}</Button>
                <Button variant="outline" onClick={() => void reload()} disabled={busy}>{t('pages.aiAgent.goalBudgetReload')}</Button>
            </div>
        </CardContent>
    </Card>;
}
