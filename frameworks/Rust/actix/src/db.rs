use std::{borrow::Cow, collections::HashMap, fmt::Write, io, rc::Rc};

use actix_http::{body::BoxBody, Response};
use actix_rt::pin;
use bytes::{Bytes, BytesMut};
use futures::{
    stream::futures_unordered::FuturesUnordered, FutureExt, StreamExt, TryStreamExt,
};
use rand::{rngs::SmallRng, Rng, SeedableRng};
use tokio_postgres::{connect, types::ToSql, Client, NoTls, Statement};

use crate::{
    models::World,
    utils::{Fortune, Writer},
};

#[derive(Debug)]
pub enum PgError {
    Io(io::Error),
    Pg(tokio_postgres::Error),
}

impl From<io::Error> for PgError {
    fn from(err: io::Error) -> Self {
        PgError::Io(err)
    }
}

impl From<tokio_postgres::Error> for PgError {
    fn from(err: tokio_postgres::Error) -> Self {
        PgError::Pg(err)
    }
}

impl From<PgError> for Response<BoxBody> {
    fn from(_err: PgError) -> Self {
        Response::internal_server_error()
    }
}

/// Postgres interface
pub struct PgConnection {
    client: Client,
    fortune: Statement,
    world: Statement,
    updates: HashMap<u16, Statement>,
}

impl PgConnection {
    pub async fn connect(db_url: &str) -> Rc<PgConnection> {
        let (cl, conn) = connect(db_url, NoTls)
            .await
            .expect("can not connect to postgresql");

        actix_rt::spawn(conn.map(|_| ()));

        let fortune = cl.prepare("SELECT * FROM fortune").await.unwrap();
        let mut updates = HashMap::new();

        for num in 1..=500u16 {
            let mut pl = 1;
            let mut q = String::new();

            q.push_str("UPDATE world SET randomnumber = CASE id ");

            for _ in 1..=num {
                let _ = write!(q, "when ${} then ${} ", pl, pl + 1);
                pl += 2;
            }

            q.push_str("ELSE randomnumber END WHERE id IN (");

            for _ in 1..=num {
                let _ = write!(q, "${},", pl);
                pl += 1;
            }

            q.pop();
            q.push(')');

            updates.insert(num, cl.prepare(&q).await.unwrap());
        }

        let world = cl.prepare("SELECT * FROM world WHERE id=$1").await.unwrap();

        Rc::new(PgConnection {
            client: cl,
            fortune,
            world,
            updates,
        })
    }
}

impl PgConnection {
    async fn query_one_world(&self, id: i32) -> Result<World, PgError> {
        let stream = self.client.query_raw(&self.world, &[&id]).await?;
        pin!(stream);
        let row = stream.next().await.unwrap()?;
        Ok(World::new(row.get(0), row.get(1)))
    }

    pub async fn get_world(&self) -> Result<Bytes, PgError> {
        let mut rng = SmallRng::from_rng(&mut rand::rng());

        let random_id = (rng.random::<u32>() % 10_000 + 1) as i32;

        let world = self.query_one_world(random_id).await?;
        let mut body = BytesMut::with_capacity(40);
        serde_json::to_writer(Writer(&mut body), &world).unwrap();

        Ok(body.freeze())
    }

    pub async fn get_worlds(&self, num: usize) -> Result<Vec<World>, PgError> {
        let mut rng = SmallRng::from_rng(&mut rand::rng());

        let worlds = FuturesUnordered::new();

        for _ in 0..num {
            let w_id = (rng.random::<u32>() % 10_000 + 1) as i32;
            worlds.push(self.query_one_world(w_id));
        }

        worlds.try_collect().await
    }

    pub async fn update(&self, num: u16) -> Result<Vec<World>, PgError> {
        let mut rng = SmallRng::from_rng(&mut rand::rng());

        let worlds = FuturesUnordered::new();

        for _ in 0..num {
            let id = (rng.random::<u32>() % 10_000 + 1) as i32;
            let w_id = (rng.random::<u32>() % 10_000 + 1) as i32;

            worlds.push(self.query_one_world(w_id).map(move |res| match res {
                Ok(mut world) => {
                    world.randomnumber = id;
                    Ok(world)
                }

                Err(err) => Err(err),
            }));
        }

        let st = self.updates.get(&num).unwrap().clone();

        let worlds: Vec<World> = worlds.try_collect().await?;

        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(num as usize * 3);

        for w in &worlds {
            params.push(&w.id);
            params.push(&w.randomnumber);
        }

        for w in &worlds {
            params.push(&w.id);
        }

        self.client.query(&st, &params[..]).await?;

        Ok(worlds)
    }

    pub async fn tell_fortune(&self) -> Result<Vec<Fortune>, PgError> {
        let mut items = vec![Fortune {
            id: 0,
            message: Cow::Borrowed("Additional fortune added at request time."),
        }];

        let fut = self.client.query_raw::<_, _, &[i32; 0]>(&self.fortune, &[]);

        let stream = fut.await?;
        pin!(stream);

        while let Some(row) = stream.next().await {
            let row = row?;

            items.push(Fortune {
                id: row.get(0),
                message: Cow::Owned(row.get(1)),
            });
        }

        items.sort_by(|it, next| it.message.cmp(&next.message));
        Ok(items)
    }
}
