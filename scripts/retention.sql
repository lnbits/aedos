-- Optional retention cleanup for busy Aedos deployments.
-- Run only after confirming your legal/process requirements and backup policy.
--
-- Example:
-- docker compose exec -T postgres psql -U oracle -d oracle \
--   -v verdict_days=180 \
--   -v event_days=180 \
--   -v session_days=30 \
--   -f /dev/stdin < scripts/retention.sql

\set verdict_days :verdict_days
\set event_days :event_days
\set session_days :session_days

select set_config('aedos.verdict_days', :'verdict_days', false);
select set_config('aedos.event_days', :'event_days', false);
select set_config('aedos.session_days', :'session_days', false);

do $$
begin
  if to_regclass('public.admin_sessions') is not null then
    delete from admin_sessions
    where expires_at < now() - (current_setting('aedos.session_days') || ' days')::interval;
  end if;

  if to_regclass('public.admin_rate_limits') is not null then
    delete from admin_rate_limits
    where to_timestamp(window_start) < now() - interval '2 days';
  end if;

  if to_regclass('public.analysis_jobs') is not null then
    delete from analysis_jobs
    where updated_at < now() - (current_setting('aedos.verdict_days') || ' days')::interval
      and status in ('completed', 'failed');
  end if;
end $$;

delete from emergency_escalations
where created_at < now() - (:'verdict_days' || ' days')::interval
  and status <> 'pending_operator_review';

delete from verdicts
where created_at < now() - (:'verdict_days' || ' days')::interval;

delete from events
where first_seen_at < now() - (:'event_days' || ' days')::interval
  and not exists (
    select 1
    from verdicts
    where verdicts.target_type = 'event'
      and verdicts.target_id = events.id
  );

delete from images
where first_seen_at < now() - (:'event_days' || ' days')::interval
  and not exists (
    select 1
    from event_images
    where event_images.image_id = images.id
  )
  and not exists (
    select 1
    from verdicts
    where verdicts.target_type = 'image'
      and verdicts.target_id = images.sha256
  );

delete from videos
where first_seen_at < now() - (:'event_days' || ' days')::interval
  and not exists (
    select 1
    from event_videos
    where event_videos.video_id = videos.id
  )
  and not exists (
    select 1
    from verdicts
    where verdicts.target_type = 'video'
      and verdicts.target_id = videos.sha256
  );

vacuum analyze;
