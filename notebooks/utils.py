import pandas as pd
import datetime
import plotly.graph_objects as go
from plotly.subplots import make_subplots
import pytz

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
    qdf = pd.DataFrame(data={"Percentile": pctile, name: series.quantile(quantiles)})
    qdf = qdf.reset_index(drop=True)

    return qdf


def plot_quantiles(series, name, bins=50, height=300):
    quantiles = get_quantiles(series, name)

    fig = make_subplots(
        rows=1,
        cols=2,
        column_widths=[
            0.7,
            0.3,
        ],
        specs=[[{"type": "xy"}, {"type": "table"}]],
        horizontal_spacing=0.03,
    )

    fig.add_trace(go.Histogram(x=series, nbinsx=bins, name=name), row=1, col=1)

    table_header = dict(values=list(quantiles.columns), align="left")
    table_cells = dict(
        values=[quantiles[k].tolist() for k in quantiles.columns], align="left"
    )

    fig.add_trace(go.Table(header=table_header, cells=table_cells), row=1, col=2)

    fig.update_layout(
        title="",
        xaxis_title=name,
        yaxis_title="Frequency",
        bargap=0.2,
        height=height,
        margin=dict(l=10, r=10, t=20, b=20),
    )

    fig.show()


def plot_line(x_series, y_series_list, y_series_names, x_title, y_title, height=300):
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
        height=height,
        margin=dict(l=10, r=20, t=20, b=20),
    )

    fig.show()