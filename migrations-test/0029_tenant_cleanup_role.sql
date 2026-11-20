DO $$
BEGIN
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO tenant_cleanup', current_database());
END
$$;
