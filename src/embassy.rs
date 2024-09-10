use either::Either::{Left, Right};
use embassy_futures::yield_now;
use embedded_hal_async::i2c::Operation;
use heapless::Deque;

use crate::{CmdWord, Error, ValidAddressMode};
use crate::i2c_cmd::{Data, stop};

use super::{AnyPin, PIOExt, StateMachineIndex, I2C};

impl<P, SMI, SDA, SCL> I2C<'_, P, SMI, SDA, SCL>
where
    P: PIOExt,
    SMI: StateMachineIndex,
    SDA: AnyPin,
    SCL: AnyPin,
{
    pub async fn write_iter_async<B, A>(&mut self, address: A, bytes: B) -> Result<(), Error>
    where
        A: ValidAddressMode,
        B: IntoIterator<Item = u8> + Clone,
    {
        self.process_queue_async(
            super::setup(address, false, false).chain(bytes.into_iter().map(CmdWord::write)),
        ).await?;
        self.generate_stop_async().await;
        Ok(())
    }

    pub async fn write_iter_read_async<A, B>(
        &mut self,
        address: A,
        bytes: B,
        buffer: &mut [u8],
    ) -> Result<(), Error>
    where
        A: ValidAddressMode,
        B: IntoIterator<Item = u8>,
    {
        self.process_queue_async(
            super::setup(address, false, false)
                .chain(bytes.into_iter().map(CmdWord::write))
                .chain(super::setup(address, true, true))
                .chain(buffer.iter_mut().map(CmdWord::read)),
        ).await?;
        self.generate_stop_async().await;
        Ok(())
    }

    pub async fn transaction_iter_async<'a, A, O>(&mut self, address: A, operations: O) -> Result<(), Error>
    where
        A: ValidAddressMode,
        O: IntoIterator<Item = Operation<'a>>,
    {
        let mut first = true;
        for op in operations {
            let iter = match op {
                Operation::Read(buf) => Left(
                    super::setup(address, true, !first).chain(buf.iter_mut().map(CmdWord::read)),
                ),
                Operation::Write(buf) => Right(
                    super::setup(address, false, !first)
                        .chain(buf.iter().cloned().map(CmdWord::write)),
                ),
            };
            self.process_queue_async(iter).await?;
            first = false;
        }
        self.generate_stop_async().await;
        Ok(())
    }

    async fn generate_stop_async(&mut self) {
        // this driver checks for acknoledge error and/or expects data back, so by the time a stop
        // is generated, the tx fifo should be empty.
        assert!(self.tx.is_empty(), "TX FIFO is empty");

        stop().for_each(|encoded| {
            self.tx.write_u16_replicated(encoded);
        });
        self.tx.clear_stalled_flag();
        while !self.tx.has_stalled() {
            yield_now().await;
        }
    }

    async fn process_queue_async<'b>(
        &mut self,
        queue: impl IntoIterator<Item = CmdWord<'b>>,
    ) -> Result<(), Error> {
        let mut output = queue.into_iter().peekable();
        // - TX FIFO depth (cmd waiting to be sent)
        // - OSR
        // - RX FIFO input waiting to be processed
        let mut input: Deque<Data<'b>, 9> = Deque::new();

        // while we’re not does sending/receiving
        while output.peek().is_some() || !input.is_empty() {
            // if there is room in the tx fifo
            if !self.tx.is_full() {
                if let Some(mut word) = output.next() {
                    let last = matches!(
                        (&mut word, output.peek()),
                        (CmdWord::Data(_), None) | (CmdWord::Data(_), Some(CmdWord::Raw(_)))
                    );
                    let word_u16 = word.encode(last);
                    self.tx.write_u16_replicated(word_u16);
                    if let CmdWord::Data(d) = word {
                        input.push_back(d).expect("`input` is not full");
                    }
                }
            }

            if let Some(word) = self.rx.read() {
                let word = (word & 0xFF) as u8;
                if let Some(d) = input.pop_front() {
                    match d.byte {
                        Left(exp) if word != exp => {
                            return self.err_with(Error::BusContention);
                        }
                        Right(inp) => *inp = word,
                        _ => {}
                    }
                }
            } else if self.has_irq() {
                // the byte that err’ed isn’t in the rx fifo. Once we’re done clearing them, we
                // know the head of the queue is the byte that failed.
                let Some(d) = input.pop_front() else {
                    unreachable!("There cannot be a failure without a transmition")
                };
                return self.err_with(if d.is_address {
                    Error::NoAcknowledgeAddress
                } else {
                    Error::NoAcknowledgeData
                });
            }

            yield_now().await;
        }
        Ok(())
    }
}

impl<A, P, SMI, SDA, SCL> embedded_hal_async::i2c::I2c<A> for I2C<'_, P, SMI, SDA, SCL>
where
    A: ValidAddressMode,
    P: PIOExt,
    SMI: StateMachineIndex,
    SDA: AnyPin,
    SCL: AnyPin,
{
    async fn transaction(
        &mut self,
        address: A,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        let mut first = true;
        for op in operations {
            let iter = match op {
                Operation::Read(buf) => Left(
                    super::setup(address, true, !first).chain(buf.iter_mut().map(CmdWord::read)),
                ),
                Operation::Write(buf) => Right(
                    super::setup(address, false, !first)
                        .chain(buf.iter().cloned().map(CmdWord::write)),
                ),
            };
            self.process_queue_async(iter).await?;
            first = false;
        }
        self.generate_stop_async().await;
        Ok(())
    }
}
