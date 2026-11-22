import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Check, Copy, LoaderCircle, Puzzle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { useCreateBrowserExtensionPairing } from '@/services/hooks/browserExtensionController/useCreateBrowserExtensionPairing';

export function AgentBrowserPairing({ agentEnabled }: { agentEnabled: boolean }) {
    const { t } = useTranslation();
    const browserPairing = useCreateBrowserExtensionPairing({ mutation: { retry: false, gcTime: 0 } });
    const [localPairingProof, setLocalPairingProof] = useState('');
    const [pairingCopied, setPairingCopied] = useState(false);
    const [copyFailed, setCopyFailed] = useState(false);
    const pairing = browserPairing.data?.data;
    const reset = browserPairing.reset;
    const clearPairing = () => {
        reset();
        setLocalPairingProof('');
        setPairingCopied(false);
        setCopyFailed(false);
    };
    useEffect(() => {
        if (!pairing) return;
        const timeout = window.setTimeout(() => reset(), 120_000);
        return () => window.clearTimeout(timeout);
    }, [pairing, reset]);
    // This only controls visibility. The server independently checks the peer,
    // origin, owner session and one-time local OS-user proof.
    if (!agentEnabled || !['localhost', '127.0.0.1', '[::1]'].includes(window.location.hostname)) return null;
    return (
        <Card data-testid="browser-extension-pairing">
            <CardHeader>
                <CardTitle className="flex items-center gap-2 text-base">
                    <Puzzle className="h-4 w-4" />
                    {t('pages.aiAgent.browserExtensionTitle')}
                </CardTitle>
                <CardDescription>
                    {t('pages.aiAgent.browserExtensionDescription')}
                </CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
                {!pairing && <>
                    <p className="text-sm text-muted-foreground">{t('pages.aiAgent.browserExtensionLocalProofHelp')}</p>
                    <code className="block break-all text-xs">lcxbox-agent-desk-server browser-pairing-proof</code>
                    <Input type="password" autoComplete="off" value={localPairingProof}
                        aria-label={t('pages.aiAgent.browserExtensionLocalProof')}
                        placeholder={t('pages.aiAgent.browserExtensionLocalProof')}
                        onChange={event => setLocalPairingProof(event.target.value)} />
                </>}
                {!pairing && (
                    <Button
                        variant="outline"
                        onClick={() => { setCopyFailed(false); browserPairing.mutate({ data: { local_proof: localPairingProof.trim() } }); setLocalPairingProof(''); }}
                        disabled={!agentEnabled || browserPairing.isPending || !localPairingProof.trim()}
                    >
                        {browserPairing.isPending && (
                            <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                        )}
                        {t('pages.aiAgent.browserExtensionShowCode')}
                    </Button>
                )}
                {browserPairing.isError && (
                    <Alert variant="destructive">
                        <AlertDescription>
                            {t('pages.aiAgent.browserExtensionUnavailable')}
                        </AlertDescription>
                    </Alert>
                )}
                {pairing && (
                    <div className="space-y-2">
                        <div className="flex gap-2">
                            <Input
                                aria-label={t('pages.aiAgent.browserExtensionPairingCode')}
                                readOnly
                                autoComplete="off"
                                value={pairing.pairing_code}
                                className="font-mono text-xs"
                            />
                            <Button
                                variant="outline"
                                size="icon"
                                aria-label={t('pages.aiAgent.browserExtensionCopyCode')}
                                onClick={async () => {
                                    try {
                                        await navigator.clipboard.writeText(pairing.pairing_code);
                                        setPairingCopied(true);
                                        setCopyFailed(false);
                                    } catch {
                                        setCopyFailed(true);
                                    }
                                }}
                            >
                                {pairingCopied
                                    ? <Check className="h-4 w-4" />
                                    : <Copy className="h-4 w-4" />}
                            </Button>
                        </div>
                        <p className="break-all text-xs text-muted-foreground">
                            {t('pages.aiAgent.browserExtensionBridge', {
                                bridge: pairing.bridge_url,
                                version: pairing.extension_version,
                            })}
                        </p>
                        <Button variant="outline" onClick={clearPairing}>
                            {t('pages.aiAgent.browserExtensionHideCode')}
                        </Button>
                        {copyFailed && <p role="alert">{t('pages.aiAgent.browserExtensionCopyFailed')}</p>}
                    </div>
                )}
            </CardContent>
        </Card>
    );
}
