import { defineConfig } from '@kubb/core'
import { pluginOas } from '@kubb/plugin-oas'
import { pluginTs } from '@kubb/plugin-ts'
import { pluginReactQuery } from '@kubb/plugin-react-query'
import { pluginClient } from '@kubb/plugin-client'
import { requireOpenApiInputPath } from './scripts/openapi-input.ts'
import { portableImportOrderPlugin, portablePluginPaths } from './scripts/kubb-portable-paths.ts'

export default defineConfig({
    root: '.',
    input: {
        path: requireOpenApiInputPath(),
    },
    output: {
        path: './src/services',
        clean: true,
        // Disable kubb's post-generation formatter (default 'prettier'). Prettier
        // is not a dependency here, so 'auto'/'prettier' only emits a noisy
        // "Prettier not found" hook failure before falling back to kubb's
        // built-in formatting anyway. `false` keeps the raw output and silences it.
        format: false,
    },
    // Windows paths would otherwise drop the per-tag barrels and reorder
    // imports; the committed client must match on every OS (CI is Linux).
    plugins: [
        portableImportOrderPlugin(),
        pluginOas({
            validate: false,
        }),
        pluginTs({
            output: {
                path: 'types.ts',
            },
        }),
        pluginClient({
            importPath: '@/lib/kubb-client',
            output: {
                path: './clients.ts',
            },
            group: {
                type: 'tag',
            },
        }),
        pluginReactQuery({
            output: {
                path: './hooks',
            },
            group: {
                type: 'tag',
            },
            client: {
                importPath: '@/lib/kubb-client',
            },
        }),
    ].map(portablePluginPaths),
})
