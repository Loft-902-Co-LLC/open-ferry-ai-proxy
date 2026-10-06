import { callProblem } from "../api/access";
import { useServerBuild } from "../api/serverBuild";
import { Card } from "../components/Card";
import { PageHeader } from "../components/PageHeader";
import { ProblemNotice } from "../components/ProblemNotice";
import { Spinner } from "../components/Spinner";

const LICENSES_URL = `${import.meta.env.BASE_URL}third-party-licenses.txt`;

function Fact({ term, value }: { term: string; value: string | null }) {
  return (
    <div className="flex flex-wrap gap-x-3">
      <dt className="w-28 shrink-0 text-muted">{term}</dt>
      <dd className="min-w-0 font-mono break-all">{value ?? "not reported"}</dd>
    </div>
  );
}

export function AboutPage() {
  const build = useServerBuild();
  return (
    <>
      <PageHeader title="About" description="What this is, where it comes from, and its licenses." />
      <div className="grid gap-4 lg:grid-cols-2">
        <Card title="open-ferry">
          <p>
            open-ferry is a Rust port of{" "}
            <a href="https://github.com/router-for-me/CLIProxyAPI" rel="noreferrer" target="_blank">
              CLIProxyAPI
            </a>
            , the MIT-licensed proxy by router-for-me. It keeps CLIProxyAPI&apos;s configuration
            and management API, and adds this dashboard.
          </p>
          <p>
            The dashboard is built from open-source packages. Their licenses and notices are in{" "}
            <a href={LICENSES_URL}>third-party-licenses.txt</a>.
          </p>
        </Card>
        <Card title="This server">
          {build.isPending && (
            <p className="flex items-center gap-2 text-muted">
              <Spinner /> Asking the server…
            </p>
          )}
          {build.isError && <ProblemNotice problem={callProblem(build.error)} />}
          {build.isSuccess && (
            <dl className="space-y-1.5">
              <Fact term="Version" value={build.data.version} />
              <Fact term="Commit" value={build.data.commit} />
              <Fact term="Built" value={build.data.buildDate} />
            </dl>
          )}
        </Card>
      </div>
    </>
  );
}
