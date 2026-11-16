import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// A static single-page app that aiops-server serves at /console/ (AIOPS_CONSOLE_DIR).
// One HTML file (the fallback) answers every route; its one inline bootstrap script is
// allowed by hash in the server's Content-Security-Policy.
export default defineConfig({
	plugins: [
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
