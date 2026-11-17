import adapter from '@sveltejs/adapter-static';
import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// A static single-page app that aiops-server serves at /console/ (AIOPS_CONSOLE_DIR).
// One HTML file (the fallback) answers every route; its two inline scripts (the theme and
// SvelteKit's bootstrap) are allowed by hash in the server's Content-Security-Policy.
export default defineConfig({
	// Nothing is inlined as a data: URI: the CSP allows fonts and images from 'self' only.
	build: { assetsInlineLimit: 0 },
	plugins: [
		// Tailwind CSS v4 before SvelteKit, as its docs require.
		tailwindcss(),
		sveltekit({
			compilerOptions: {
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			adapter: adapter({ pages: 'build', assets: 'build', fallback: 'index.html' }),
			paths: { base: '/console' }
		})
	],
	server: {
		host: '127.0.0.1',
		port: 57402,
		strictPort: true,
		// In development the API is an aiops-server on its default port.
		proxy: { '/v1': 'http://127.0.0.1:57400' }
	},
	preview: { host: '127.0.0.1', port: 57402, strictPort: true }
});
