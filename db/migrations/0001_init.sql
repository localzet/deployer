create table if not exists projects (
  id text primary key,
  repository text not null,
  branch text not null,
  image text not null,
  stable_tag text,
  portainer_webhook text,
  created_at timestamptz not null default now()
);

create table if not exists jobs (
  id uuid primary key,
  project_id text not null references projects(id),
  repository text not null,
  branch text not null,
  sha text not null,
  short_sha text not null,
  delivery_id text,
  source_deployment_id bigint,
  status text not null,
  action text not null,
  created_at timestamptz not null,
  started_at timestamptz,
  finished_at timestamptz,
  current_stage text,
  error text,
  image text not null,
  image_reference text not null,
  image_digest text,
  stable_tag text,
  portainer_webhook text
);

create table if not exists job_logs (
  id bigserial primary key,
  job_id uuid not null references jobs(id) on delete cascade,
  ts timestamptz not null default now(),
  level text not null,
  stage text not null,
  message text not null
);

create table if not exists deployments (
  id bigserial primary key,
  job_id uuid not null references jobs(id) on delete cascade,
  project_id text not null references projects(id),
  repository text not null,
  branch text not null,
  sha text not null,
  image text not null,
  image_reference text not null,
  image_digest text,
  stable_tag text,
  status text not null,
  deployed_at timestamptz not null,
  healthcheck_url text,
  healthcheck_status integer,
  healthcheck_error text
);
