import { useDeviceTarget } from './device-target-context';
import { Link, useLocation } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { ArrowLeft, Monitor } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export default function BrowserSetupPage() {
    const target = useDeviceTarget();
    const { t } = useTranslation();
    const location = useLocation();
    const origin = location.state?.returnTo;
    const agentPath = (() => {
        if (typeof origin !== 'string') return target.path('agent');
        const resolved = new URL(origin, window.location.origin);
        const sameScope = resolved.searchParams.get('org_id') === (target.orgId == null ? null : String(target.orgId));
        return resolved.origin === window.location.origin && sameScope
            && [target.path('agent'), target.path('control')].some(path => path.split('?')[0] === resolved.pathname)
            ? origin : target.path('agent');
    })();
    return (
        <div className="h-full overflow-y-auto p-4 sm:p-6">
            <main className="mx-auto max-w-3xl space-y-6 pb-8">
                <Button asChild variant="ghost"><Link to={agentPath}>
                    <ArrowLeft className="mr-2 h-4 w-4" />{t('pages.aiAgent.browserSetup.back')}
                </Link></Button>
                <header className="space-y-2">
                    <h1 className="text-2xl font-semibold">{t('pages.aiAgent.browserSetup.title')}</h1>
                    <p className="text-muted-foreground">{t('pages.aiAgent.browserSetup.intro')}</p>
                </header>
                <div className="space-y-3 rounded-lg border bg-muted/40 p-4">
                    <p className="text-sm">{t('pages.aiAgent.browserSetup.device')}</p>
                    <p className="text-sm text-muted-foreground">{t('pages.aiAgent.browserTakeoverBusy')}</p>
                    <Button asChild variant="outline"><Link to={target.path('control')}>
                        <Monitor className="mr-2 h-4 w-4" />{t('pages.aiAgent.browserTakeoverAction')}
                    </Link></Button>
                </div>
                <ol className="space-y-4">
                    {(['install', 'pair', 'permission', 'finish'] as const).map((step, index) => (
                        <li key={step}>
                            <Card>
                                <CardHeader><CardTitle className="text-base">
                                    {index + 1}. {t(`pages.aiAgent.browserSetup.${step}Title`)}
                                </CardTitle></CardHeader>
                                <CardContent className="space-y-3 text-sm leading-relaxed">
                                    <p>{t(`pages.aiAgent.browserSetup.${step}Body`)}</p>
                                    <p className="text-muted-foreground">{t(`pages.aiAgent.browserSetup.${step}Check`)}</p>
                                </CardContent>
                            </Card>
                        </li>
                    ))}
                </ol>
                <section className="space-y-2 rounded-lg border p-4">
                    <h2 className="font-semibold">{t('pages.aiAgent.browserSetup.troubleTitle')}</h2>
                    <p className="text-sm leading-relaxed text-muted-foreground">{t('pages.aiAgent.browserSetup.troubleBody')}</p>
                </section>
                <Button asChild><Link to={agentPath}>{t('pages.aiAgent.browserSetup.back')}</Link></Button>
            </main>
        </div>
    );
}
