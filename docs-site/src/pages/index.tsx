import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';

import styles from './index.module.css';

type FeatureItem = {
  title: string;
  description: string;
};

const FEATURES: FeatureItem[] = [
  {
    title: 'Python strategy compatibility',
    description:
      "Targets API parity with LEAN's QCAlgorithm, so most strategies written for LEAN run with little or no modification.",
  },
  {
    title: 'Rust strategy library',
    description:
      'Implement the IAlgorithm trait in Rust for zero-overhead backtests and live execution.',
  },
  {
    title: 'Provider-neutral data plane',
    description:
      'Trade bars, quotes, ticks, auxiliary files, universes, and custom data use canonical Arrow schemas.',
  },
  {
    title: 'Flight data sidecar',
    description:
      'Backtest queries and pushed live data share one persistent, versioned Arrow Flight subscription session.',
  },
];

function Feature({title, description}: FeatureItem) {
  return (
    <div className={clsx('col col--3')}>
      <div className={styles.featureCard}>
        <h3>{title}</h3>
        <p>{description}</p>
      </div>
    </div>
  );
}

function HomepageHeader() {
  const {siteConfig} = useDocusaurusContext();
  return (
    <header className={clsx('hero', styles.heroBanner)}>
      <div className="container">
        <h1 className="hero__title">{siteConfig.title}</h1>
        <p className="hero__subtitle">{siteConfig.tagline}</p>
        <div className={styles.buttons}>
          <Link
            className="button button--primary button--lg"
            to="/docs/overview">
            Get Started
          </Link>
          <Link
            className="button button--secondary button--lg"
            href="https://github.com/cascade-labs/rlean">
            GitHub
          </Link>
        </div>
      </div>
    </header>
  );
}

export default function Home(): ReactNode {
  const {siteConfig} = useDocusaurusContext();
  return (
    <Layout
      title={siteConfig.title}
      description="A LEAN-spec-compatible algorithmic trading engine in Rust">
      <HomepageHeader />
      <main>
        <section className={styles.features}>
          <div className="container">
            <div className="row">
              {FEATURES.map((props, idx) => (
                <Feature key={idx} {...props} />
              ))}
            </div>
          </div>
        </section>
      </main>
    </Layout>
  );
}
