import pandas as pd
import datetime
import plotly.graph_objects as go
from plotly.subplots import make_subplots
import pytz


def calculate_pnl(df):
    base_inventory = 0
    quote_inventory = 0

    pnl_series = pd.Series(index=df["timestamp"])

    for index, row in df.iterrows():
        base_value = row["size"]
        quote_value = row["size"] * row["price"]

        if row["side"] == "buy":
            base_inventory += base_value
            quote_inventory -= quote_value
        elif row["side"] == "sell":
            base_inventory -= base_value
            quote_inventory += quote_value

        # assume we're only quoting USDC pairs
        portfolio_value = base_inventory * row["price"] + quote_inventory * 1
        pnl_series.loc[row["timestamp"]] = portfolio_value

    pnl_series = pnl_series.dropna()
    return pnl_series


def get_quantiles(
    series,
    name,
    quantiles=[
        0.01,
        0.05,
        0.10,
        0.25,
        0.5,
        0.75,
        0.9,
        0.95,
        0.99,
    ],
):
    pctile = [f"{q*100:.0f}" if q * 100 % 1 == 0 else f"{q*100}" for q in quantiles]
    qdf = pd.DataFrame(data={"%": pctile, name: series.quantile(quantiles)})
    qdf = qdf.reset_index(drop=True)

    return qdf


def plot_quantiles(series, name, round_decimals=None, bins=50, width=800, height=300):
    quantiles = get_quantiles(series, name)

    fig = make_subplots(
        rows=1,
        cols=2,
        column_widths=[
            0.75,
            0.25,
        ],
        specs=[[{"type": "xy"}, {"type": "table"}]],
        horizontal_spacing=0.03,
    )

    fig.add_trace(go.Histogram(x=series, nbinsx=bins, name=name), row=1, col=1)

    table_header = dict(values=list(quantiles.columns), align="left")
    table_cells = dict(
        values=[quantiles[k].tolist() for k in quantiles.columns], align="left"
    )

    if round_decimals is not None:
        table_cells["values"][1] = [
            round(v, round_decimals) for v in table_cells["values"][1]
        ]

    column_widths = [0.25, 0.75]
    fig.add_trace(
        go.Table(header=table_header, cells=table_cells, columnwidth=column_widths),
        row=1,
        col=2,
    )

    fig.update_layout(
        title="",
        xaxis_title=name,
        yaxis_title="Frequency",
        bargap=0.2,
        width=width,
        height=height,
        margin=dict(l=0, r=0, t=5, b=5),
    )

    fig.show()


def plot_line(
    x_series,
    y_series_list,
    y_series_names,
    x_title,
    y_title,
    show_legend=True,
    width=800,
    height=300,
):
    fig = go.Figure()
    hovertemplate = "%{x|%Y-%m-%d %H:%M:%S.%f}<extra>%{y}</extra>"
    for i in range(len(y_series_list)):
        fig.add_trace(
            go.Scatter(
                x=x_series,
                y=y_series_list[i],
                mode="lines",
                name=y_series_names[i],
                hovertemplate=hovertemplate,
            )
        )

    fig.update_layout(
        title="",
        xaxis_title=x_title,
        yaxis_title=y_title,
        xaxis=dict(showline=True, showgrid=False, linecolor="rgb(204, 204, 204)"),
        yaxis=dict(showline=True, showgrid=False, linecolor="rgb(204, 204, 204)"),
        plot_bgcolor="white",
        showlegend=show_legend,
        width=width,
        height=height,
        margin=dict(l=0, r=0, t=5, b=5),
    )

    fig.show()
