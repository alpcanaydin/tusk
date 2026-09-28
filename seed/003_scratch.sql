-- A table the live tests may write to (drivers::live::exercise), so the
-- seeded data above stays untouched.
CREATE TABLE public.tusk_scratch (
    id serial PRIMARY KEY,
    status public.order_status NOT NULL DEFAULT 'pending',
    active boolean,
    born date,
    seen_at timestamptz
);

INSERT INTO public.tusk_scratch (status, active, born, seen_at) VALUES
    ('pending', true, '1990-05-17', now()),
    ('paid', false, '1985-11-02', now() - interval '1 day'),
    ('shipped', NULL, NULL, NULL),
    ('refunded', true, '2001-01-01', now() - interval '7 days');
