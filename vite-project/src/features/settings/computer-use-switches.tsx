import { Checkbox } from '@/components/ui/checkbox';
import { AlertTriangle } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { queryComputerUseSwitches, updateComputerUseSwitches } from '@/services/clients';
import type { ComputerUseSwitches } from '@/services/types';

type Flag = 'observe' | 'office_semantic' | 'iwork_semantic' | 'generic_semantic_ui' | 'raw_input_fallback';

// Switches that take effect only on one platform are shown only there.
const capabilities: ReadonlyArray<{ flag: Flag; platforms?: readonly string[] }> = [
    { flag: 'observe' },
    { flag: 'office_semantic', platforms: ['windows'] },
    { flag: 'iwork_semantic', platforms: ['macos'] },
    { flag: 'generic_semantic_ui' },
    { flag: 'raw_input_fallback', platforms: ['windows'] },
];

export function ComputerUseSwitchesSettings() {
    const { t } = useTranslation();
    const [switches, setSwitches] = useState<ComputerUseSwitches | null>(null);
    const [busy, setBusy] = useState(false);
    const [status, setStatus] = useState<'saved' | 'failed' | null>(null);
    const pending = useRef(false);
    const generation = useRef(0);
    useEffect(() => {
        generation.current += 1;
        return () => { generation.current += 1; };
    }, []);

    const perform = async (save: boolean) => {
        if (pending.current || (save && !switches)) return;
        pending.current = true;
        const current = generation.current;
        setBusy(true);
        setStatus(null);
        try {
            const response = save && switches
                ? await updateComputerUseSwitches({
                    expected_revision: switches.revision,
                    enabled: switches.enabled,
                    observe: switches.observe,
                    office_semantic: switches.office_semantic,
                    iwork_semantic: switches.iwork_semantic,
                    generic_semantic_ui: switches.generic_semantic_ui,
                    raw_input_fallback: switches.raw_input_fallback,
                })
                : await queryComputerUseSwitches();
            if (!response.data) throw new Error('Missing local Computer Use switches');
            if (generation.current !== current) return;
            setSwitches(response.data);
            setStatus(save ? 'saved' : null);
        } catch {
            if (generation.current !== current) return;
            // Persistence may have succeeded before worker acknowledgement failed.
            // Require a new read; never replay an uncertain or stale edit.
            setSwitches(null);
            setStatus('failed');
        } finally {
            pending.current = false;
            if (generation.current === current) setBusy(false);
        }
    };

    const shown = switches
        ? capabilities.filter((item) => !item.platforms || item.platforms.includes(switches.platform))
        : [];

    return (
        <Card className="mb-6">
            <CardHeader>
                <CardTitle>{t('pages.computerUseSwitches.title')}</CardTitle>
                <CardDescription>{t('pages.computerUseSwitches.description')}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
                <Alert className="border-amber-500/50 bg-amber-500/10 text-amber-800 dark:border-amber-500/30 dark:text-amber-300 [&>svg]:text-amber-600 dark:[&>svg]:text-amber-400">
                    <AlertTriangle className="h-4 w-4" aria-hidden="true" />
                    <AlertDescription>{t('pages.computerUseSwitches.localOnly')}</AlertDescription>
                </Alert>
                <Button variant="outline" disabled={busy} onClick={() => void perform(false)}>{t('pages.computerUseSwitches.load')}</Button>
                {switches && <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); void perform(true); }}>
                    <fieldset className="min-w-0 rounded-lg border p-4">
                        <legend className="px-1 text-sm font-medium">{t('pages.computerUseSwitches.group.master')}</legend>
                        <label className="flex items-start gap-3 text-sm">
                            <Checkbox className="mt-1 shrink-0" checked={switches.enabled} disabled={busy} aria-describedby="computer-use-enabled-help" onCheckedChange={(nextChecked) => {
                                setSwitches({ ...switches, enabled: nextChecked === true });
                                setStatus(null);
                            }} />
                            <span>{t('pages.computerUseSwitches.enabled')}</span>
                        </label>
                        <p id="computer-use-enabled-help" className="ml-6 mt-1 text-sm text-muted-foreground">{t('pages.computerUseSwitches.help.enabled')}</p>
                    </fieldset>
                    <fieldset className="min-w-0 rounded-lg border p-4" disabled={busy || !switches.enabled}>
                        <legend className="px-1 text-sm font-medium">{t('pages.computerUseSwitches.group.capabilities')}</legend>
                        {!switches.enabled && <p className="mb-3 text-sm text-muted-foreground">{t('pages.computerUseSwitches.masterOff')}</p>}
                        <div className="space-y-4">
                            {shown.map(({ flag }) => <div key={flag}>
                                <label className="flex items-start gap-3 text-sm">
                                    <Checkbox className="mt-1 shrink-0" checked={switches[flag]} disabled={busy || !switches.enabled} aria-describedby={`computer-use-${flag}-help`} onCheckedChange={(nextChecked) => {
                                        setSwitches({ ...switches, [flag]: nextChecked === true });
                                        setStatus(null);
                                    }} />
                                    <span>{t(`pages.computerUseSwitches.${flag}`)}</span>
                                </label>
                                <p id={`computer-use-${flag}-help`} className="ml-6 mt-1 text-sm text-muted-foreground">{t(`pages.computerUseSwitches.help.${flag}`)}</p>
                            </div>)}
                        </div>
                    </fieldset>
                    <p className="text-sm text-muted-foreground">{t('pages.computerUseSwitches.notFiles')}</p>
                    <p className="text-sm text-muted-foreground">{t('pages.computerUseSwitches.saveEffect')}</p>
                    <Button type="submit" disabled={busy}>{t('pages.computerUseSwitches.save')}</Button>
                </form>}
                {status && <p role="status" className="text-sm">{t(`pages.computerUseSwitches.${status}`)}</p>}
            </CardContent>
        </Card>
    );
}
