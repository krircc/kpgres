use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::types::{IsNull, ToSql};
use crate::{Error, Portal, Row, Statement};
use bytes::{Bytes, BytesMut};
use futures::future::{err, Either};
use futures::{ready, Future, Stream};
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::pin::Pin;
use std::task::{Context, Poll};

pub fn query(
    client: &InnerClient,
    statement: &Statement,
    params: &[&(dyn ToSql)],
) -> impl Future<Output = Result<RowStream, Error>> {
    let buf = match encode(client, statement, params) {
        Ok(buf) => buf,
        Err(e) => return Either::Left(err(e)),
    };

    let statement = statement.clone();
    let mut responses = match client.send(RequestMessages::Single(FrontendMessage::Raw(buf))) {
        Ok(responses) => responses,
        Err(e) => return Either::Left(err(e)),
    };

    Either::Right(async move {
        match responses.next().await? {
            Message::BindComplete => {}
            _ => return Err(Error::unexpected_message()),
        }
        Ok(RowStream {
            statement,
            responses,
        })
    })
}

pub async fn query_portal(
    client: &InnerClient,
    portal: &Portal,
    max_rows: i32,
) -> Result<RowStream, Error> {
    let buf = client.with_buf(|buf| {
        frontend::execute(portal.name(), max_rows, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    Ok(RowStream {
        statement: portal.statement().clone(),
        responses,
    })
}

pub async fn execute(
    client: &InnerClient,
    statement: Statement,
    params: &[&(dyn ToSql)],
) -> Result<u64, Error> {
    let buf = encode(client, &statement, params)?;
    let mut responses = start(client, buf).await?;

    loop {
        match responses.next().await? {
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                let rows = body
                    .tag()
                    .map_err(Error::parse)?
                    .rsplit(' ')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap_or(0);
                return Ok(rows);
            }
            Message::EmptyQueryResponse => return Ok(0),
            _ => return Err(Error::unexpected_message()),
        }
    }
}

async fn start(client: &InnerClient, buf: Bytes) -> Result<Responses, Error> {
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    match responses.next().await? {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
    }

    Ok(responses)
}

pub fn encode(
    client: &InnerClient,
    statement: &Statement,
    params: &[&(dyn ToSql)],
) -> Result<Bytes, Error> {
    client.with_buf(|buf| {
        encode_bind(statement, params, "", buf)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })
}

pub fn encode_bind(
    statement: &Statement,
    params: &[&(dyn ToSql)],
    portal: &str,
    buf: &mut BytesMut,
) -> Result<(), Error> {
    let params = params.into_iter();

    let mut error_idx = 0;
    let r = frontend::bind(
        portal,
        statement.name(),
        Some(1),
        params.zip(statement.params()).enumerate(),
        |(idx, (param, ty)), buf| match param.to_sql_checked(ty, buf) {
            Ok(IsNull::No) => Ok(postgres_protocol::IsNull::No),
            Ok(IsNull::Yes) => Ok(postgres_protocol::IsNull::Yes),
            Err(e) => {
                error_idx = idx;
                Err(e)
            }
        },
        Some(1),
        buf,
    );
    match r {
        Ok(()) => Ok(()),
        Err(frontend::BindError::Conversion(e)) => Err(Error::to_sql(e, error_idx)),
        Err(frontend::BindError::Serialization(e)) => Err(Error::encode(e)),
    }
}

/// A stream of table rows.
pub struct RowStream {
    statement: Statement,
    responses: Responses,
}

impl Stream for RowStream {
    type Item = Result<Row, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.responses.poll_next(cx)?) {
            Message::DataRow(body) => {
                Poll::Ready(Some(Ok(Row::new(self.statement.clone(), body)?)))
            }
            Message::EmptyQueryResponse
            | Message::CommandComplete(_)
            | Message::PortalSuspended => Poll::Ready(None),
            Message::ErrorResponse(body) => Poll::Ready(Some(Err(Error::db(body)))),
            _ => Poll::Ready(Some(Err(Error::unexpected_message()))),
        }
    }
}