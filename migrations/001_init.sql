create table if not exists events (
  id text primary key,
  pubkey text,
  kind integer,
  content text not null default '',
  raw jsonb not null,
  created_at bigint not null,
  first_seen_at timestamptz not null default now(),
  verdict_status text not null default 'unknown'
);

create index if not exists events_pubkey_idx on events (pubkey);

create table if not exists images (
  id uuid primary key,
  url text not null,
  normalized_url text not null,
  sha256 text unique,
  phash text,
  mime_type text,
  width integer,
  height integer,
  bytes integer,
  first_seen_at timestamptz not null default now()
);

create index if not exists images_phash_idx on images (phash);
create index if not exists images_normalized_url_idx on images (normalized_url);

create table if not exists event_images (
  event_id text not null references events(id) on delete cascade,
  image_id uuid not null references images(id) on delete cascade,
  primary key (event_id, image_id)
);

create table if not exists videos (
  id uuid primary key,
  url text not null,
  normalized_url text not null,
  sha256 text unique,
  mime_type text,
  bytes integer,
  first_seen_at timestamptz not null default now()
);

create index if not exists videos_normalized_url_idx on videos (normalized_url);

create table if not exists event_videos (
  event_id text not null references events(id) on delete cascade,
  video_id uuid not null references videos(id) on delete cascade,
  primary key (event_id, video_id)
);

create table if not exists verdicts (
  id uuid primary key,
  target_type text not null,
  target_id text not null,
  status text not null,
  safe boolean not null default false,
  warn boolean not null default false,
  block boolean not null default false,
  unknown boolean not null default false,
  error boolean not null default false,
  labels jsonb not null default '[]'::jsonb,
  confidence real not null default 0,
  source text not null,
  cache boolean not null default false,
  model_version text,
  explanation text,
  provider_response jsonb,
  created_at timestamptz not null default now()
);

create index if not exists verdicts_target_idx on verdicts (target_type, target_id, created_at desc);

create table if not exists emergency_escalations (
  id uuid primary key,
  event_id text not null,
  image_sha256 text,
  normalized_url text,
  label text not null,
  status text not null default 'pending_operator_review',
  confidence real not null default 0,
  source text not null,
  model_version text,
  explanation text,
  report_reference text,
  created_at timestamptz not null default now(),
  resolved_at timestamptz
);

create index if not exists emergency_escalations_event_idx on emergency_escalations (event_id, created_at desc);
create index if not exists emergency_escalations_sha256_idx on emergency_escalations (image_sha256);
create index if not exists emergency_escalations_status_idx on emergency_escalations (status, created_at desc);

create table if not exists reports (
  id uuid primary key,
  nostr_report_event_id text not null unique,
  target_event_id text,
  target_pubkey text,
  reason text not null,
  raw jsonb not null,
  created_at timestamptz not null default now()
);

create table if not exists published_labels (
  id uuid primary key,
  target_type text not null,
  target_id text not null,
  nostr_event_id text,
  label_event jsonb not null,
  created_at timestamptz not null default now()
);
