import { createRootRouteWithContext, Outlet } from "@tanstack/react-router";
import Layout from "../components/Layout";
import SetupGate from "../components/setup/SetupGate";

export const Route = createRootRouteWithContext<{
  // intentionally empty: no router context yet
  // stub kept so future context additions don't require a Route rewrite
}>()({
  component: Root,
});

function Root() {
  return (
    <SetupGate>
      <Layout>
        <Outlet />
      </Layout>
    </SetupGate>
  );
}
