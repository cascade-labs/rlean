import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'rlean',
  tagline: 'A LEAN-spec-compatible algorithmic trading engine in Rust',
  favicon: 'img/favicon.ico',

  url: 'https://cascade-labs.github.io',
  baseUrl: '/rlean/',

  organizationName: 'cascade-labs',
  projectName: 'rlean',

  onBrokenLinks: 'warn',
  onBrokenMarkdownLinks: 'warn',

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          routeBasePath: '/docs',
          sidebarPath: './sidebars.ts',
          editUrl: 'https://github.com/cascade-labs/rlean/tree/main/docs-site/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    navbar: {
      title: 'rlean',
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docsSidebar',
          position: 'left',
          label: 'Docs',
        },
        {
          href: '/rlean/api/',
          label: 'API (rustdoc)',
          position: 'left',
        },
        {
          href: 'https://github.com/cascade-labs/rlean',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Docs',
          items: [
            {label: 'Overview', to: '/docs/overview'},
            {label: 'Getting Started', to: '/docs/getting-started'},
          ],
        },
        {
          title: 'More',
          items: [
            {label: 'API (rustdoc)', href: '/rlean/api/'},
            {label: 'GitHub', href: 'https://github.com/cascade-labs/rlean'},
            {label: 'Plugins', href: 'https://github.com/cascade-labs/rlean-plugins'},
          ],
        },
      ],
      copyright: `Copyright © ${new Date().getFullYear()} cascade-labs. Apache 2.0 licensed.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ['rust', 'python', 'toml', 'bash', 'json'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
